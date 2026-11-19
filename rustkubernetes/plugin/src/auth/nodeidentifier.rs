//! Port of `pkg/auth/nodeidentifier`: which node a user is.

use k8s_apiserver::authentication::UserInfo;

const NODE_USER_NAME_PREFIX: &str = "system:node:";
/// `user.NodesGroup`
pub const NODES_GROUP: &str = "system:nodes";

/// `NodeIdentity`: `(node name, is a node)`. A node identity needs the
/// `system:node:` user name prefix *and* the `system:nodes` group; the name
/// may be empty (an unknown node).
pub fn node_identity(u: &UserInfo) -> (String, bool) {
    let Some(name) = u.name.strip_prefix(NODE_USER_NAME_PREFIX) else { return (String::new(), false) };
    if !u.in_group(NODES_GROUP) {
        return (String::new(), false);
    }
    (name.to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities() {
        assert_eq!(node_identity(&UserInfo::new("system:node:n1", &["system:nodes"])), ("n1".into(), true));
        assert_eq!(node_identity(&UserInfo::new("system:node:", &["system:nodes"])), ("".into(), true));
        assert_eq!(node_identity(&UserInfo::new("system:node:n1", &["system:authenticated"])), ("".into(), false));
        assert_eq!(node_identity(&UserInfo::new("n1", &["system:nodes"])), ("".into(), false));
    }
}
