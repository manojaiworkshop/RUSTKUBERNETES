//! Port of `pkg/kubelet/apis` (`well_known_labels.go`): the Node labels a
//! kubelet may set on itself.

/// `kubeletLabels`
pub const KUBELET_LABELS: &[&str] = &[
    "kubernetes.io/hostname",
    "topology.kubernetes.io/zone",
    "topology.kubernetes.io/region",
    "failure-domain.beta.kubernetes.io/zone",
    "failure-domain.beta.kubernetes.io/region",
    "beta.kubernetes.io/instance-type",
    "node.kubernetes.io/instance-type",
    "kubernetes.io/os",
    "kubernetes.io/arch",
    "beta.kubernetes.io/os",
    "beta.kubernetes.io/arch",
];

/// `kubeletLabelNamespaces`
pub const KUBELET_LABEL_NAMESPACES: &[&str] = &["kubelet.kubernetes.io", "node.kubernetes.io"];

/// The prefix of a label key (`example.com` of `example.com/x`).
pub fn label_namespace(key: &str) -> &str {
    key.split_once('/').map(|(ns, _)| ns).unwrap_or("")
}

/// `ns` is `base` or a subdomain of it.
pub fn in_namespace(ns: &str, base: &str) -> bool {
    ns == base || ns.ends_with(&format!(".{base}"))
}

/// A label in the `kubernetes.io` or `k8s.io` namespaces.
pub fn is_kubernetes_label(key: &str) -> bool {
    let ns = label_namespace(key);
    in_namespace(ns, "kubernetes.io") || in_namespace(ns, "k8s.io")
}

/// `IsKubeletLabel`
pub fn is_kubelet_label(key: &str) -> bool {
    KUBELET_LABELS.contains(&key) || KUBELET_LABEL_NAMESPACES.iter().any(|n| in_namespace(label_namespace(key), n))
}

/// The kubelet's `--node-labels` check: unknown `kubernetes.io` / `k8s.io` labels are refused.
pub fn validate_node_labels(keys: &[&str]) -> Result<(), String> {
    let mut unknown: Vec<&str> = keys.iter().copied().filter(|k| is_kubernetes_label(k) && !is_kubelet_label(k)).collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort();
    Err(format!(
        "unknown 'kubernetes.io' or 'k8s.io' labels specified with --node-labels: [{}]\n--node-labels in the 'kubernetes.io' namespace must begin with an allowed prefix ({}) or be in the specifically allowed set ({})",
        unknown.join(" "),
        KUBELET_LABEL_NAMESPACES.join(", "),
        KUBELET_LABELS.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_labels() {
        assert!(validate_node_labels(&["kubernetes.io/hostname", "node.kubernetes.io/pool", "example.com/x"]).is_ok());
        assert!(validate_node_labels(&["node-role.kubernetes.io/worker"]).unwrap_err().contains("node-role.kubernetes.io/worker"));
    }
}
