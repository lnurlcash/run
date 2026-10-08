//! Provisioning by commit. A mint is a directory in a git repository of
//! Kubernetes configuration:
//!
//! ```text
//! mints/<name>/mint.yaml        what Run was asked for (not applied)
//! mints/<name>/resources.yaml   PVC, StatefulSet, Service, Ingress
//! argocd/applicationset.yaml    one Argo CD Application per mints/* directory
//! ```
//!
//! Run only commits and pushes; Argo CD makes the cluster match. Manifests
//! are built as JSON values and written as YAML whose strings are all
//! double-quoted (JSON's escaping is valid YAML), so nothing a user names can
//! break out of a field.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

/// Names that can't be a mint's subdomain.
const RESERVED: &[&str] = &[
    "www",
    "api",
    "run",
    "admin",
    "app",
    "mail",
    "dash",
    "dashboard",
    "status",
    "docs",
    "blog",
    "mint",
    "argocd",
    "grafana",
    "electrs",
    "bitcoin",
    "lsp",
    "static",
    "beta",
];

/// The cluster-wide settings every mint is rendered with.
#[derive(Debug, Clone)]
pub struct Platform {
    /// The lnurl-mint-rust image, pinned: `lnurlcash/lnurl-mint-rust:0.1.0`.
    pub image: String,
    /// Mints live at `<name>.<domain>`.
    pub domain: String,
    pub namespace: String,
    /// bitcoin, signet, …
    pub network: String,
    /// The shared electrs every mint's node syncs from.
    pub electrum_url: String,
    /// The LSPS2 provider, pubkey@host:port.
    pub lsp_node: Option<String>,
    /// cert-manager issuer for the Ingress's certificate.
    pub cert_issuer: String,
    /// A wildcard certificate's secret for `*.<domain>`; the issuer is then
    /// not asked per mint.
    pub tls_secret: Option<String>,
    pub ingress_class: String,
    pub storage: String,
    pub storage_class: Option<String>,
    /// The git repository Argo CD watches, for the ApplicationSet.
    pub repo_url: String,
    pub branch: String,
}

/// What one mint is.
#[derive(Debug, Clone, PartialEq)]
pub struct Mint {
    pub name: String,
    /// The owner's nostr pubkey, hex.
    pub owner: String,
    pub enabled: bool,
}

/// A subdomain-safe name: 3–30 of a-z, 0-9 and inner dashes.
pub fn check_name(name: &str) -> Result<()> {
    ensure!(
        (3..=30).contains(&name.len()),
        "a mint's name is 3 to 30 characters"
    );
    ensure!(
        name.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "a mint's name has only a-z, 0-9 and dashes"
    );
    ensure!(
        !name.starts_with('-') && !name.ends_with('-'),
        "a mint's name can't start or end with a dash"
    );
    ensure!(!RESERVED.contains(&name), "that name is reserved");
    Ok(())
}

impl Platform {
    pub fn host(&self, mint: &Mint) -> String {
        format!("{}.{}", mint.name, self.domain)
    }

    /// `mint.yaml`: the request, for people reading the repository.
    pub fn spec(&self, mint: &Mint) -> Value {
        json!({
            "apiVersion": "run.lnurlcash.com/v1",
            "kind": "Mint",
            "metadata": {"name": mint.name},
            "spec": {
                "owner": crate::nostr::npub(&mint.owner),
                "enabled": mint.enabled,
                "image": self.image,
                "domain": self.host(mint),
                "network": self.network,
                "chain": "electrum",
                "liquidity": {"lsp": if self.lsp_node.is_some() { "lsps2" } else { "none" }},
            }
        })
    }

    /// The Kubernetes resources one mint runs as.
    pub fn resources(&self, mint: &Mint) -> Vec<Value> {
        let name = &mint.name;
        let host = self.host(mint);
        let labels = json!({
            "app.kubernetes.io/name": "lnurl-mint",
            "app.kubernetes.io/instance": name,
            "app.kubernetes.io/managed-by": "lnurlcash-run",
        });
        let meta = |kind_name: String| json!({"name": kind_name, "namespace": self.namespace, "labels": labels});

        let mut pvc_meta = meta(format!("{name}-data"));
        // the seed and the channel state: Argo CD must never prune or delete
        // this, not even when the mint's directory is removed
        pvc_meta["annotations"] = json!({
            "argocd.argoproj.io/sync-options": "Delete=false,Prune=false",
        });
        let mut pvc_spec = json!({
            "accessModes": ["ReadWriteOnce"],
            "resources": {"requests": {"storage": self.storage}},
        });
        if let Some(class) = &self.storage_class {
            pvc_spec["storageClassName"] = json!(class);
        }
        let pvc = json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": pvc_meta, "spec": pvc_spec});

        let mut env = vec![
            json!({"name": "BASE_URL", "value": format!("https://{host}")}),
            json!({"name": "NETWORK", "value": self.network}),
            json!({"name": "DATA_DIR", "value": "/data"}),
            json!({"name": "LISTEN", "value": "0.0.0.0:8111"}),
            json!({"name": "LN_LISTEN", "value": "0.0.0.0:9735"}),
            json!({"name": "ELECTRUM_URL", "value": self.electrum_url}),
            json!({"name": "TITLE", "value": format!("{name} · LNURLcash mint")}),
        ];
        if let Some(lsp) = &self.lsp_node {
            env.push(json!({"name": "LSP_NODE", "value": lsp}));
        }
        let probe = |delay: u32| {
            json!({
                "httpGet": {"path": "/.well-known/lnurlp/_", "port": 8111},
                "initialDelaySeconds": delay, "periodSeconds": 20, "timeoutSeconds": 10, "failureThreshold": 6,
            })
        };
        let statefulset = json!({
            "apiVersion": "apps/v1",
            "kind": "StatefulSet",
            "metadata": meta(name.clone()),
            "spec": {
                "replicas": if mint.enabled { 1 } else { 0 },
                "serviceName": name,
                "selector": {"matchLabels": {"app.kubernetes.io/instance": name}},
                "persistentVolumeClaimRetentionPolicy": {"whenDeleted": "Retain", "whenScaled": "Retain"},
                "template": {
                    "metadata": {"labels": labels},
                    "spec": {
                        // SIGTERM writes the channel state: give it time,
                        // a SIGKILL can force-close a channel
                        "terminationGracePeriodSeconds": 90,
                        "automountServiceAccountToken": false,
                        "securityContext": {"runAsUser": 1000, "runAsGroup": 1000, "fsGroup": 1000, "runAsNonRoot": true},
                        // a read-only root: SQLite's temp files go to an emptyDir
                        "volumes": [
                            {"name": "data", "persistentVolumeClaim": {"claimName": format!("{name}-data")}},
                            {"name": "tmp", "emptyDir": {"sizeLimit": "64Mi"}},
                        ],
                        "containers": [{
                            "name": "lnurl-mint",
                            "image": self.image,
                            "env": env,
                            "ports": [
                                {"name": "http", "containerPort": 8111},
                                {"name": "lightning", "containerPort": 9735},
                            ],
                            "volumeMounts": [
                                {"name": "data", "mountPath": "/data"},
                                {"name": "tmp", "mountPath": "/tmp"},
                            ],
                            "readinessProbe": probe(10),
                            "livenessProbe": probe(120),
                            "resources": {
                                "requests": {"cpu": "50m", "memory": "128Mi"},
                                "limits": {"memory": "512Mi"},
                            },
                            "securityContext": {
                                "allowPrivilegeEscalation": false,
                                "readOnlyRootFilesystem": true,
                                "capabilities": {"drop": ["ALL"]},
                            },
                        }],
                    },
                },
            },
        });

        let service = json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": meta(name.clone()),
            "spec": {
                "selector": {"app.kubernetes.io/instance": name},
                "ports": [{"name": "http", "port": 80, "targetPort": "http"}],
            },
        });

        let mut ingress_meta = meta(name.clone());
        let tls = match &self.tls_secret {
            Some(secret) => json!({"hosts": [format!("*.{}", self.domain)], "secretName": secret}),
            None => {
                ingress_meta["annotations"] =
                    json!({"cert-manager.io/cluster-issuer": self.cert_issuer});
                json!({"hosts": [host], "secretName": format!("{name}-tls")})
            }
        };
        let ingress = json!({
            "apiVersion": "networking.k8s.io/v1",
            "kind": "Ingress",
            "metadata": ingress_meta,
            "spec": {
                "ingressClassName": self.ingress_class,
                "tls": [tls],
                "rules": [{
                    "host": host,
                    "http": {"paths": [{
                        "path": "/", "pathType": "Prefix",
                        "backend": {"service": {"name": name, "port": {"name": "http"}}},
                    }]},
                }],
            },
        });

        vec![pvc, statefulset, service, ingress]
    }

    /// `argocd/applicationset.yaml`: an Application per `mints/*`, applying
    /// everything there but `mint.yaml`. Automated sync, never pruning: a
    /// mint goes away by an operator's hand, not by a missing file.
    pub fn applicationset(&self) -> Value {
        json!({
            "apiVersion": "argoproj.io/v1alpha1",
            "kind": "ApplicationSet",
            "metadata": {"name": "lnurlcash-run-mints", "namespace": "argocd"},
            "spec": {
                "goTemplate": true,
                "generators": [{"git": {
                    "repoURL": self.repo_url,
                    "revision": self.branch,
                    "directories": [{"path": "mints/*"}],
                }}],
                "template": {
                    "metadata": {"name": "mint-{{.path.basename}}"},
                    "spec": {
                        "project": "default",
                        "source": {
                            "repoURL": self.repo_url,
                            "targetRevision": self.branch,
                            "path": "{{.path.path}}",
                            "directory": {"exclude": "mint.yaml"},
                        },
                        "destination": {"server": "https://kubernetes.default.svc", "namespace": self.namespace},
                        "syncPolicy": {
                            "automated": {"prune": false, "selfHeal": true},
                            "syncOptions": ["CreateNamespace=true"],
                        },
                    },
                },
            },
        })
    }
}

// ---- YAML ----

/// Block-style YAML of a JSON value. Every string is double-quoted with
/// JSON's escaping, which YAML reads identically.
pub fn to_yaml(value: &Value) -> String {
    let mut out = String::new();
    write_yaml(value, 0, &mut out);
    out
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("null".into()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(Value::String(s.clone()).to_string()),
        Value::Array(a) if a.is_empty() => Some("[]".into()),
        Value::Object(o) if o.is_empty() => Some("{}".into()),
        _ => None,
    }
}

/// A key as is when YAML reads it back as that same string, else quoted.
fn yaml_key(key: &str) -> String {
    const WORDS: &[&str] = &[
        "y", "n", "yes", "no", "true", "false", "on", "off", "null", "~",
    ];
    let plain = key.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c))
        && !WORDS.contains(&key.to_ascii_lowercase().as_str());
    if plain {
        key.to_string()
    } else {
        Value::String(key.to_string()).to_string()
    }
}

fn write_yaml(value: &Value, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                let key = yaml_key(key);
                match scalar(v) {
                    Some(s) => out.push_str(&format!("{pad}{key}: {s}\n")),
                    None => {
                        out.push_str(&format!("{pad}{key}:\n"));
                        write_yaml(v, indent + 2, out);
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                match scalar(item) {
                    Some(s) => out.push_str(&format!("{pad}- {s}\n")),
                    None => {
                        // the item's first line goes after the dash
                        let mut inner = String::new();
                        write_yaml(item, indent + 2, &mut inner);
                        let inner = inner.trim_start_matches(' ');
                        out.push_str(&format!("{pad}- {inner}"));
                    }
                }
            }
        }
        other => out.push_str(&format!("{pad}{}\n", scalar(other).unwrap_or_default())),
    }
}

fn documents(values: &[Value]) -> String {
    values.iter().map(to_yaml).collect::<Vec<_>>().join("---\n")
}

// ---- git ----

/// A clone of the GitOps repository. Operations are serialized: one
/// worktree, one commit at a time.
pub struct Repo {
    dir: PathBuf,
    url: String,
    branch: String,
    lock: Mutex<()>,
}

const AUTHOR: &str = "lnurlcash-run <run@lnurlcash.com>";

impl Repo {
    pub fn new(dir: PathBuf, url: String, branch: String) -> Self {
        Repo {
            dir,
            url,
            branch,
            lock: Mutex::new(()),
        }
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        run_git(Some(&self.dir), args)
    }

    /// Clone, or bring the clone up to date with the remote.
    fn sync(&self) -> Result<()> {
        if !self.dir.join(".git").exists() {
            std::fs::create_dir_all(self.dir.parent().unwrap_or(Path::new(".")))?;
            let dir = self.dir.to_string_lossy();
            run_git(
                None,
                &["clone", "--branch", &self.branch, "--", &self.url, &dir],
            )
            .or_else(|_| {
                // an empty repository has no branch yet
                run_git(None, &["clone", "--", &self.url, &dir])?;
                run_git(Some(&self.dir), &["checkout", "-B", &self.branch])
            })
            .context("could not clone the GitOps repository")?;
            return Ok(());
        }
        // a remote without the branch yet (nothing pushed) is fine
        if self
            .git(&[
                "ls-remote",
                "--exit-code",
                "--heads",
                "origin",
                &self.branch,
            ])
            .is_ok()
        {
            self.git(&["fetch", "origin", &self.branch])?;
            self.git(&["reset", "--hard", &format!("origin/{}", self.branch)])?;
        }
        Ok(())
    }

    /// Run `change` on an up-to-date worktree, then commit everything it
    /// changed with `message` and push. Retries once when the remote moved
    /// in between. Returns the commit, or `None` if nothing changed.
    pub fn commit(
        &self,
        message: &str,
        change: impl Fn(&Path) -> Result<()>,
    ) -> Result<Option<String>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        for attempt in 0..2 {
            self.sync()?;
            change(&self.dir)?;
            self.git(&["add", "-A"])?;
            if self.git(&["status", "--porcelain"])?.trim().is_empty() {
                return Ok(None);
            }
            self.git(&[
                "-c",
                "user.name=lnurlcash-run",
                "-c",
                "user.email=run@lnurlcash.com",
                "commit",
                "-q",
                "--author",
                AUTHOR,
                "-m",
                message,
            ])?;
            match self.git(&["push", "-q", "origin", &format!("HEAD:{}", self.branch)]) {
                Ok(_) => {
                    return Ok(Some(
                        self.git(&["rev-parse", "--short", "HEAD"])?
                            .trim()
                            .to_string(),
                    ));
                }
                Err(e) if attempt == 0 => {
                    log::warn!("push refused, retrying on the new remote state: {e:#}")
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }
}

fn run_git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    // never prompt: a missing credential is an error, not a hang
    let out = cmd
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("could not run git")?;
    if !out.status.success() {
        // the repository URL may carry a token: keep it out of errors
        bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            redact(&String::from_utf8_lossy(&out.stderr))
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `https://user:token@host` → `https://***@host`.
fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_inclusive(char::is_whitespace) {
        match (word.find("://"), word.find('@')) {
            (Some(scheme), Some(at)) if at > scheme => {
                out.push_str(&word[..scheme + 3]);
                out.push_str("***");
                out.push_str(&word[at..]);
            }
            _ => out.push_str(word),
        }
    }
    out
}

// ---- what Run does ----

pub struct GitOps {
    pub platform: Platform,
    pub repo: Repo,
}

impl GitOps {
    fn write_mint(&self, dir: &Path, mint: &Mint) -> Result<()> {
        let mint_dir = dir.join("mints").join(&mint.name);
        std::fs::create_dir_all(&mint_dir)?;
        std::fs::write(
            mint_dir.join("mint.yaml"),
            to_yaml(&self.platform.spec(mint)),
        )?;
        std::fs::write(
            mint_dir.join("resources.yaml"),
            documents(&self.platform.resources(mint)),
        )?;
        let argocd = dir.join("argocd");
        std::fs::create_dir_all(&argocd)?;
        std::fs::write(
            argocd.join("applicationset.yaml"),
            to_yaml(&self.platform.applicationset()),
        )?;
        Ok(())
    }

    /// Commit a new mint. Refuses a name the repository already has.
    pub fn provision(&self, mint: &Mint) -> Result<Option<String>> {
        check_name(&mint.name)?;
        let message = format!(
            "run: provision {} for {}",
            mint.name,
            crate::nostr::npub(&mint.owner)
        );
        self.repo.commit(&message, |dir| {
            if dir.join("mints").join(&mint.name).exists() {
                bail!("a mint named {} exists already", mint.name);
            }
            self.write_mint(dir, mint)
        })
    }

    /// Re-render an existing mint: scaled up or down, or onto a new
    /// platform setting (an image upgrade is this, after changing MINT_IMAGE).
    pub fn update(&self, mint: &Mint, why: &str) -> Result<Option<String>> {
        let message = format!("run: {why} {}", mint.name);
        self.repo.commit(&message, |dir| {
            if !dir.join("mints").join(&mint.name).exists() {
                bail!("no mint named {} in the repository", mint.name);
            }
            self.write_mint(dir, mint)
        })
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn platform(repo_url: &str) -> Platform {
        Platform {
            image: "lnurlcash/lnurl-mint-rust:0.1.0".into(),
            domain: "run.lnurlcash.com".into(),
            namespace: "lnurlcash-mints".into(),
            network: "signet".into(),
            electrum_url: "tcp://electrs.bitcoin.svc:50001".into(),
            lsp_node: Some("02ab@lsp.example:9735".into()),
            cert_issuer: "letsencrypt".into(),
            tls_secret: None,
            ingress_class: "nginx".into(),
            storage: "2Gi".into(),
            storage_class: None,
            repo_url: repo_url.into(),
            branch: "main".into(),
        }
    }

    fn mint(name: &str) -> Mint {
        Mint {
            name: name.into(),
            owner: "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e".into(),
            enabled: true,
        }
    }

    #[test]
    fn names() {
        check_name("satoshi").unwrap();
        check_name("meetup-zrh").unwrap();
        for bad in [
            "ab",
            "Satoshi",
            "-x-y",
            "x-y-",
            "a_b",
            "www",
            "a.b",
            &"x".repeat(31),
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn yaml_round_trips_through_json_escaping() {
        let value = json!({"a": "x: \"y\"\n#z", "b": [1, {"c": true, "d": []}, "e"], "f": {}, "g": {"h": null}, "on": 1, "x: y": 2});
        let yaml = to_yaml(&value);
        assert_eq!(
            yaml,
            "a: \"x: \\\"y\\\"\\n#z\"\nb:\n  - 1\n  - c: true\n    d: []\n  - \"e\"\nf: {}\ng:\n  h: null\n\"on\": 1\n\"x: y\": 2\n"
        );
    }

    #[test]
    fn resources_carry_the_platform() {
        let p = platform("https://git.example/k8s.git");
        let [pvc, sts, _svc, ingress] =
            <[Value; 4]>::try_from(p.resources(&mint("satoshi"))).unwrap();
        assert_eq!(
            pvc["metadata"]["annotations"]["argocd.argoproj.io/sync-options"],
            "Delete=false,Prune=false"
        );
        assert_eq!(sts["spec"]["replicas"], 1);
        let env = &sts["spec"]["template"]["spec"]["containers"][0]["env"];
        let get = |k: &str| {
            env.as_array()
                .unwrap()
                .iter()
                .find(|e| e["name"] == k)
                .map(|e| e["value"].clone())
        };
        assert_eq!(
            get("BASE_URL"),
            Some(json!("https://satoshi.run.lnurlcash.com"))
        );
        assert_eq!(
            get("ELECTRUM_URL"),
            Some(json!("tcp://electrs.bitcoin.svc:50001"))
        );
        assert_eq!(get("LSP_NODE"), Some(json!("02ab@lsp.example:9735")));
        assert_eq!(get("BITCOIND_RPC"), None);
        assert_eq!(
            ingress["spec"]["rules"][0]["host"],
            "satoshi.run.lnurlcash.com"
        );
        let off = Mint {
            enabled: false,
            ..mint("satoshi")
        };
        assert_eq!(p.resources(&off)[1]["spec"]["replicas"], 0);
    }

    #[test]
    fn tokens_stay_out_of_errors() {
        assert_eq!(
            redact("fatal: unable to access 'https://run:glpat-secret@gitlab.com/x.git/': 403"),
            "fatal: unable to access 'https://***@gitlab.com/x.git/': 403"
        );
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        run_git(Some(dir), args).unwrap()
    }

    /// Against a real (bare, local) remote: a commit per change, nothing
    /// for no change, a refused duplicate, and a commit someone else pushed
    /// in between rebased onto.
    #[test]
    fn provisioning_commits_and_pushes() {
        let tmp = std::env::temp_dir().join(format!("lnurlcash-run-gitops-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let remote = tmp.join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "-q", "--bare", "-b", "main"]);
        let url = remote.to_string_lossy().into_owned();
        let ops = GitOps {
            platform: platform(&url),
            repo: Repo::new(tmp.join("clone"), url.clone(), "main".into()),
        };

        let first = ops.provision(&mint("satoshi")).unwrap();
        assert!(first.is_some());
        assert!(
            ops.provision(&mint("satoshi"))
                .unwrap_err()
                .to_string()
                .contains("exists already")
        );
        assert_eq!(
            ops.update(&mint("satoshi"), "re-render").unwrap(),
            None,
            "nothing changed"
        );

        // someone else pushes meanwhile
        let other = tmp.join("other");
        run_git(None, &["clone", "-q", &url, &other.to_string_lossy()]).unwrap();
        std::fs::write(other.join("README.md"), "hand-written\n").unwrap();
        git(&other, &["add", "-A"]);
        git(
            &other,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "docs",
            ],
        );
        git(&other, &["push", "-q", "origin", "main"]);

        let off = Mint {
            enabled: false,
            ..mint("satoshi")
        };
        assert!(ops.update(&off, "disable").unwrap().is_some());
        ops.provision(&mint("meetup-zrh")).unwrap();

        let log = git(&remote, &["log", "--format=%an|%s", "main"]);
        let subjects: Vec<_> = log.lines().collect();
        assert_eq!(subjects.len(), 4, "{log}");
        assert!(
            subjects[0].starts_with("lnurlcash-run|run: provision meetup-zrh for npub1"),
            "{log}"
        );
        assert_eq!(subjects[1], "lnurlcash-run|run: disable satoshi");
        assert_eq!(subjects[2], "t|docs");
        let resources = git(&remote, &["show", "main:mints/satoshi/resources.yaml"]);
        assert!(resources.contains("replicas: 0"), "{resources}");
        assert!(resources.matches("---\n").count() == 3);
        let appset = git(&remote, &["show", "main:argocd/applicationset.yaml"]);
        assert!(appset.contains("exclude: \"mint.yaml\""));
        let spec = git(&remote, &["show", "main:mints/meetup-zrh/mint.yaml"]);
        assert!(
            spec.contains("npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg"),
            "{spec}"
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
