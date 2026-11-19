//! Bootstrap tokens: `kubeadm token create|list|delete|generate`,
//! `kubeadm init phase bootstrap-token` (`phases/bootstraptoken/{clusterinfo,node}`)
//! and the token-based discovery of `kubeadm join` (`discovery/token`).

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine;
use k8s_api::core::v1::{ConfigMap, Secret};
use k8s_api::rbac::v1::{ClusterRole, ClusterRoleBinding, PolicyRule, Role, RoleBinding, RoleRef, Subject};
use k8s_apimachinery::meta::ObjectMeta;
use k8s_client_go::{Api, Client};

pub const NODE_BOOTSTRAP_TOKEN_AUTH_GROUP: &str = "system:bootstrappers:kubeadm:default-node-token";
pub const DEFAULT_USAGES: &[&str] = &["signing", "authentication"];
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// `GenerateBootstrapToken`: `[a-z0-9]{6}.[a-z0-9]{16}`.
pub fn generate_token() -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let rng = ring::rand::SystemRandom::new();
    let rand = |n: usize| -> String {
        (0..n)
            .map(|_| loop {
                let mut b = [0u8; 1];
                ring::rand::SecureRandom::fill(&rng, &mut b).unwrap();
                // Rejection sampling keeps the distribution uniform over 36 characters.
                if b[0] < 252 {
                    break CHARS[(b[0] % 36) as usize] as char;
                }
            })
            .collect()
    };
    format!("{}.{}", rand(6), rand(16))
}

/// `ParseToken`
pub fn parse_token(t: &str) -> Result<(&str, &str)> {
    let ok = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    match t.split_once('.') {
        Some((id, secret)) if ok(id, 6) && ok(secret, 16) => Ok((id, secret)),
        _ => bail!("the bootstrap token {t:?} was not of the form \"\\\\A([a-z0-9]{{6}})\\\\.([a-z0-9]{{16}})\\\\z\""),
    }
}

/// `BootstrapToken.ToSecret`
pub fn token_secret(token: &str, ttl: Duration, usages: &[String], groups: &[String], description: &str) -> Result<Secret> {
    let (id, secret) = parse_token(token)?;
    let mut data = BTreeMap::new();
    let mut put = |k: &str, v: &str| {
        data.insert(k.to_string(), B64.encode(v));
    };
    put("token-id", id);
    put("token-secret", secret);
    if !description.is_empty() {
        put("description", description);
    }
    if !ttl.is_zero() {
        let exp = chrono::Utc::now() + chrono::Duration::from_std(ttl)?;
        put("expiration", &exp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
    for u in usages {
        put(&format!("usage-bootstrap-{u}"), "true");
    }
    if !groups.is_empty() {
        put("auth-extra-groups", &groups.join(","));
    }
    Ok(Secret {
        metadata: ObjectMeta { name: Some(format!("bootstrap-token-{id}")), namespace: Some("kube-system".into()), ..Default::default() },
        type_: Some("bootstrap.kubernetes.io/token".into()),
        data: Some(data),
        ..Secret::new(Default::default())
    })
}

/// `kubeadm token create`
pub async fn create(client: &Client, token: &str, ttl: Duration, usages: &[String], groups: &[String], description: &str) -> Result<()> {
    for g in groups {
        if !g.starts_with("system:bootstrappers:") {
            bail!("bootstrap group {g:?} is invalid (must match \\Asystem:bootstrappers:[a-z0-9:-]{{0,255}}[a-z0-9]\\z)");
        }
    }
    let s = token_secret(token, ttl, usages, groups, description)?;
    let api: Api<Secret> = Api::namespaced(client.clone(), "kube-system");
    match api.create(&s).await {
        Ok(_) => Ok(()),
        Err(e) if e.is_already_exists() => bail!("a token with id {:?} already exists", parse_token(token)?.0),
        Err(e) => Err(e.into()),
    }
}

/// `kubeadm token list` (text printer).
pub async fn list(client: &Client) -> Result<String> {
    let api: Api<Secret> = Api::namespaced(client.clone(), "kube-system");
    let secrets = api.list(&k8s_client_go::ListParams::default().fields("type=bootstrap.kubernetes.io/token")).await?;
    let get = |s: &Secret, k: &str| {
        s.data
            .as_ref()
            .and_then(|d| d.get(k))
            .and_then(|v| B64.decode(v).ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };
    let mut out = String::new();
    let mut rows = vec![];
    for s in &secrets.items {
        if s.type_.as_deref() != Some("bootstrap.kubernetes.io/token") {
            continue;
        }
        let (id, secret) = (get(s, "token-id"), get(s, "token-secret"));
        if id.is_empty() {
            continue;
        }
        let exp = get(s, "expiration");
        let (ttl, expires) = match chrono::DateTime::parse_from_rfc3339(&exp) {
            Ok(t) => (short_human_duration((t.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds()), exp.clone()),
            Err(_) => ("<forever>".to_string(), "<never>".to_string()),
        };
        let usages: Vec<String> = s
            .data
            .iter()
            .flatten()
            .filter_map(|(k, _)| k.strip_prefix("usage-bootstrap-").filter(|_| get(s, k) == "true").map(str::to_string))
            .collect();
        let usages = if usages.is_empty() { "<none>".to_string() } else { usages.join(",") };
        let desc = get(s, "description");
        let groups = get(s, "auth-extra-groups");
        rows.push(format!(
            "{id}.{secret}\t{ttl:<9}\t{expires}\t{usages:<22}\t{:<56}\t{}\n",
            if desc.is_empty() { "<none>" } else { &desc },
            if groups.is_empty() { "<none>" } else { &groups }
        ));
    }
    let lines: Vec<Vec<String>> = std::iter::once("TOKEN\tTTL\tEXPIRES\tUSAGES\tDESCRIPTION\tEXTRA GROUPS".to_string())
        .chain(rows.iter().map(|r| r.trim_end_matches('\n').to_string()))
        .map(|l| l.split('\t').map(str::to_string).collect())
        .collect();
    let widths: Vec<usize> = (0..6).map(|c| lines.iter().map(|l| l.get(c).map_or(0, |s| s.chars().count())).max().unwrap_or(0)).collect();
    for l in &lines {
        let mut line = String::new();
        for (i, cell) in l.iter().enumerate() {
            if i + 1 == l.len() {
                line.push_str(cell);
            } else {
                let w = (widths[i] + 3).max(10);
                line.push_str(&format!("{cell:<w$}"));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    if rows.is_empty() {
        return Ok(String::new());
    }
    Ok(out)
}

/// `duration.ShortHumanDuration`
fn short_human_duration(seconds: i64) -> String {
    if seconds < -1 {
        "<invalid>".into()
    } else if seconds < 0 {
        "0s".into()
    } else if seconds < 60 {
        format!("{seconds}s")
    } else if seconds / 60 < 60 {
        format!("{}m", seconds / 60)
    } else if seconds / 3600 < 24 {
        format!("{}h", seconds / 3600)
    } else if seconds / 3600 < 24 * 365 {
        format!("{}d", seconds / 86400)
    } else {
        format!("{}y", seconds / 86400 / 365)
    }
}

/// `kubeadm token delete <id|token>`
pub async fn delete(client: &Client, token_or_id: &str) -> Result<()> {
    let id = match token_or_id.split_once('.') {
        Some(_) => parse_token(token_or_id)?.0.to_string(),
        None => {
            if token_or_id.len() != 6 || !token_or_id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) {
                bail!(
                    "given token didn't match pattern \"\\\\A([a-z0-9]{{6}})\\\\z\" or \"\\\\A([a-z0-9]{{6}})\\\\.([a-z0-9]{{16}})\\\\z\""
                );
            }
            token_or_id.to_string()
        }
    };
    let api: Api<Secret> = Api::namespaced(client.clone(), "kube-system");
    api.delete(&format!("bootstrap-token-{id}")).await.with_context(|| format!("failed to delete bootstrap token {id:?}"))?;
    println!("bootstrap token {id:?} deleted");
    Ok(())
}

async fn create_or_update_cluster_role(client: &Client, r: ClusterRole) -> Result<()> {
    let api: Api<ClusterRole> = Api::all(client.clone());
    match api.create(&r).await {
        Err(e) if e.is_already_exists() => {
            let mut cur = api.get(r.metadata.name.as_deref().unwrap()).await?;
            cur.rules = r.rules;
            api.replace(&cur).await?;
        }
        other => {
            other?;
        }
    }
    Ok(())
}

async fn create_or_update_crb(client: &Client, name: &str, role: &str, subjects: Vec<Subject>) -> Result<()> {
    let api: Api<ClusterRoleBinding> = Api::all(client.clone());
    let b = ClusterRoleBinding {
        metadata: ObjectMeta { name: Some(name.into()), ..Default::default() },
        role_ref: RoleRef { api_group: "rbac.authorization.k8s.io".into(), kind: "ClusterRole".into(), name: role.into() },
        subjects: Some(subjects),
        ..ClusterRoleBinding::new(Default::default())
    };
    match api.create(&b).await {
        Err(e) if e.is_already_exists() => {
            let mut cur = api.get(name).await?;
            cur.subjects = b.subjects;
            api.replace(&cur).await?;
        }
        other => {
            other?;
        }
    }
    Ok(())
}

/// `kubeadm init phase bootstrap-token`: cluster-info for discovery, and the RBAC
/// that lets bootstrap tokens post CSRs that the csrapprover approves.
pub async fn init_phase(client: &Client, server: &str, ca_pem: &str) -> Result<()> {
    let group = Subject::group(NODE_BOOTSTRAP_TOKEN_AUTH_GROUP);
    create_or_update_crb(client, "kubeadm:kubelet-bootstrap", "system:node-bootstrapper", vec![group.clone()]).await?;
    println!("[bootstrap-token] Configured RBAC rules to allow Node Bootstrap tokens to post CSRs in order for nodes to get long term certificate credentials");
    create_or_update_cluster_role(
        client,
        ClusterRole {
            metadata: ObjectMeta { name: Some("kubeadm:get-nodes".into()), ..Default::default() },
            rules: Some(vec![PolicyRule::resources(&[""], &["nodes"], &["get"])]),
            ..ClusterRole::new(Default::default())
        },
    )
    .await?;
    create_or_update_crb(client, "kubeadm:get-nodes", "kubeadm:get-nodes", vec![group.clone()]).await?;
    println!("[bootstrap-token] Configured RBAC rules to allow Node Bootstrap tokens to get nodes");
    create_or_update_crb(
        client,
        "kubeadm:node-autoapprove-bootstrap",
        "system:certificates.k8s.io:certificatesigningrequests:nodeclient",
        vec![group],
    )
    .await?;
    println!("[bootstrap-token] Configured RBAC rules to allow the csrapprover controller automatically approve CSRs from a Node Bootstrap Token");
    create_or_update_crb(
        client,
        "kubeadm:node-autoapprove-certificate-rotation",
        "system:certificates.k8s.io:certificatesigningrequests:selfnodeclient",
        vec![Subject::group("system:nodes")],
    )
    .await?;
    println!("[bootstrap-token] Configured RBAC rules to allow certificate rotation for all node client certificates in the cluster");

    // cluster-info: the cluster's address and CA, readable anonymously.
    println!("[bootstrap-token] Creating the \"cluster-info\" ConfigMap in the \"kube-public\" namespace");
    let kubeconfig = format!(
        "apiVersion: v1\nclusters:\n- cluster:\n    certificate-authority-data: {}\n    server: {server}\n  name: \"\"\ncontexts: null\ncurrent-context: \"\"\nkind: Config\nusers: null\n",
        B64.encode(ca_pem)
    );
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), "kube-public");
    let mut cm =
        ConfigMap::new(ObjectMeta { name: Some("cluster-info".into()), namespace: Some("kube-public".into()), ..Default::default() });
    cm.data = Some([("kubeconfig".to_string(), kubeconfig)].into_iter().collect());
    match cms.create(&cm).await {
        Err(e) if e.is_already_exists() => {
            let mut cur = cms.get("cluster-info").await?;
            let d = cur.data.get_or_insert_with(Default::default);
            d.insert("kubeconfig".into(), cm.data.unwrap()["kubeconfig"].clone());
            cms.replace(&cur).await?;
        }
        other => {
            other?;
        }
    }
    let roles: Api<Role> = Api::namespaced(client.clone(), "kube-public");
    let mut rule = PolicyRule::resources(&[""], &["configmaps"], &["get"]);
    rule.resource_names = Some(vec!["cluster-info".into()]);
    let role = Role {
        metadata: ObjectMeta {
            name: Some("kubeadm:bootstrap-signer-clusterinfo".into()),
            namespace: Some("kube-public".into()),
            ..Default::default()
        },
        rules: Some(vec![rule]),
        ..Role::new(Default::default())
    };
    if let Err(e) = roles.create(&role).await {
        if !e.is_already_exists() {
            return Err(e.into());
        }
    }
    let rbs: Api<RoleBinding> = Api::namespaced(client.clone(), "kube-public");
    let rb = RoleBinding {
        metadata: ObjectMeta {
            name: Some("kubeadm:bootstrap-signer-clusterinfo".into()),
            namespace: Some("kube-public".into()),
            ..Default::default()
        },
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".into(),
            kind: "Role".into(),
            name: "kubeadm:bootstrap-signer-clusterinfo".into(),
        },
        subjects: Some(vec![Subject::user("system:anonymous")]),
        ..RoleBinding::new(Default::default())
    };
    if let Err(e) = rbs.create(&rb).await {
        if !e.is_already_exists() {
            return Err(e.into());
        }
    }
    Ok(())
}

/// `pubkeypin.Hash`: `sha256:<hex of SHA-256 over the CA's SubjectPublicKeyInfo>`.
pub fn ca_cert_hash(ca_pem: &[u8]) -> Result<String> {
    let block = x509_parser::pem::Pem::iter_from_buffer(ca_pem).next().context("no CA certificate")??;
    let cert = block.parse_x509()?;
    let d = ring::digest::digest(&ring::digest::SHA256, cert.tbs_certificate.subject_pki.raw);
    Ok(format!("sha256:{}", d.as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

/// The join command printed by `kubeadm token create --print-join-command`.
pub fn join_command(endpoint: &str, token: &str, ca_pem: &[u8]) -> Result<String> {
    let host = endpoint.trim_start_matches("https://");
    Ok(format!("kubeadm join {host} --token {token} --discovery-token-ca-cert-hash {} ", ca_cert_hash(ca_pem)?))
}

/// `discovery/token.RetrieveValidatedConfigInfo`: reads `cluster-info` without
/// trusting the server, checks the token's JWS over it and the CA pin, and
/// returns (server, CA PEM).
pub async fn discover(endpoint: &str, token: &str, ca_hashes: &[String], unsafe_skip_ca_verification: bool) -> Result<(String, String)> {
    let (id, secret) = parse_token(token)?;
    if ca_hashes.is_empty() && !unsafe_skip_ca_verification {
        bail!("discovery-token-ca-cert-hash must be specified (or use --discovery-token-unsafe-skip-ca-verification)");
    }
    let server = if endpoint.starts_with("https://") { endpoint.to_string() } else { format!("https://{endpoint}") };
    let mut cfg = k8s_client_go::Config::new(&server);
    cfg.tls.insecure = true;
    let insecure = Client::new(cfg)?;
    let cms: Api<ConfigMap> = Api::namespaced(insecure, "kube-public");
    let cm = cms.get("cluster-info").await.context("failed to request the cluster-info ConfigMap")?;
    let data = cm.data.unwrap_or_default();
    let kubeconfig = data.get("kubeconfig").context("there is no kubeconfig key in the cluster-info ConfigMap")?;
    let sig = data
        .get(&format!("jws-kubeconfig-{id}"))
        .with_context(|| format!("token id {id:?} is invalid for this cluster or it has expired. Use \"kubeadm token create\" on the control-plane node to create a new valid token"))?;
    if !detached_jws_valid(sig, kubeconfig, id, secret) {
        bail!("failed to verify JWS signature of received cluster info object, can't trust this API Server");
    }
    let y: serde_yaml::Value = serde_yaml::from_str(kubeconfig)?;
    let cluster = &y["clusters"][0]["cluster"];
    let ca = B64.decode(cluster["certificate-authority-data"].as_str().context("cluster-info has no CA")?)?;
    let ca_pem = String::from_utf8(ca)?;
    if !unsafe_skip_ca_verification {
        let got = ca_cert_hash(ca_pem.as_bytes())?;
        if !ca_hashes.iter().any(|h| h.eq_ignore_ascii_case(&got)) {
            bail!("cluster CA found in cluster-info ConfigMap is invalid: none of the public keys \"{got}\" are pinned");
        }
    }
    let server = cluster["server"].as_str().unwrap_or(&server).to_string();
    // Second fetch over TLS with the CA we now trust.
    let mut cfg = k8s_client_go::Config::new(&server);
    cfg.tls.ca_data = Some(ca_pem.clone().into_bytes());
    let secure: Api<ConfigMap> = Api::namespaced(Client::new(cfg)?, "kube-public");
    secure.get("cluster-info").await.context("failed to request the cluster-info ConfigMap over a verified connection")?;
    println!("[discovery] Cluster info signature and contents are valid and TLS certificate validates against pinned roots, will use API Server {server:?}");
    Ok((server, ca_pem))
}

/// Recomputes the detached HS256 JWS (`header..sig`) and compares.
fn detached_jws_valid(sig: &str, content: &str, id: &str, secret: &str) -> bool {
    let Some((header, mac)) = sig.split_once("..") else { return false };
    let Ok(h) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(header) else { return false };
    let Ok(h): Result<serde_json::Value, _> = serde_json::from_slice(&h) else { return false };
    if h["alg"] != "HS256" || h["kid"] != id {
        return false;
    }
    let mut key = secret.as_bytes().to_vec();
    key.resize(key.len().max(32), 0);
    let input = format!("{header}.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(content));
    let Ok(mac) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(mac) else { return false };
    ring::hmac::verify(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &key), input.as_bytes(), &mac).is_ok()
}

/// `kubeadm join`'s `bootstrap-kubelet.conf`: the token as credential.
pub fn bootstrap_kubeconfig(server: &str, ca_pem: &str, token: &str) -> String {
    format!(
        "apiVersion: v1\nkind: Config\nclusters:\n- name: kubernetes\n  cluster:\n    certificate-authority-data: {}\n    server: {server}\ncontexts:\n- name: tls-bootstrap-token-user@kubernetes\n  context:\n    cluster: kubernetes\n    user: tls-bootstrap-token-user\ncurrent-context: tls-bootstrap-token-user@kubernetes\nusers:\n- name: tls-bootstrap-token-user\n  user:\n    token: {token}\n",
        B64.encode(ca_pem)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        let t = generate_token();
        let (id, s) = parse_token(&t).unwrap();
        assert_eq!((id.len(), s.len()), (6, 16));
        assert!(parse_token("abc.def").is_err());
        let sec = token_secret("abcdef.0123456789abcdef", Duration::from_secs(3600), &["signing".into()], &[], "d").unwrap();
        let d = sec.data.unwrap();
        assert_eq!(B64.decode(&d["usage-bootstrap-signing"]).unwrap(), b"true");
        assert!(d.contains_key("expiration"));
        assert_eq!(sec.metadata.name.as_deref(), Some("bootstrap-token-abcdef"));
        // The kcm's bootstrap signer and join discovery agree on the JWS.
        let sig = "eyJhbGciOiJIUzI1NiIsImtpZCI6ImFiY2RlZiJ9..f_akjwCRL9bIa1GF6CvBk7CaS19Tn3RN-uWQ-FFayFc";
        assert!(detached_jws_valid(sig, "foo", "abcdef", "abcdef0123456789"));
        assert!(!detached_jws_valid(sig, "bar", "abcdef", "abcdef0123456789"));
        assert_eq!(short_human_duration(23 * 3600 + 59 * 60), "23h");
    }
}
