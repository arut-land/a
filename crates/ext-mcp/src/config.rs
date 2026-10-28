//! Stdio process scope is an explicit host experiment, never remote lifetime policy.

use ext::{Cx, Refused, Table};

use crate::{ServerName, Transport};

#[derive(Clone, Copy, Default)]
pub(crate) enum Scope {
    #[default]
    Session,
    Workspace,
    User,
}

impl Scope {
    pub(crate) fn read(
        table: &impl Table,
        name: &ServerName,
        transport: &Transport,
        sharing: bool,
    ) -> Result<Self, Refused> {
        let key = format!("{name}_scope");
        let Some(value) = table.text(&key)? else {
            return Ok(Self::Session);
        };
        if matches!(transport, Transport::Http(_)) {
            return Err(Refused(format!(
                "ext.mcp.{key}: scope controls stdio processes only"
            )));
        }
        let scope = match value {
            "session" => Self::Session,
            "workspace" => Self::Workspace,
            "user" => Self::User,
            _ => {
                return Err(Refused(format!(
                    "ext.mcp.{key}: expected session, workspace or user"
                )));
            }
        };
        if scope.shared() && !sharing {
            return Err(Refused(format!(
                "ext.mcp.{key}: requires experimental_stdio_sharing = true"
            )));
        }
        Ok(scope)
    }

    pub(crate) fn shared(self) -> bool {
        !matches!(self, Self::Session)
    }

    pub(crate) fn key(self, cx: &Cx) -> ext::Scope {
        match self {
            Self::Session => ext::Scope::Session(cx.session),
            Self::Workspace => ext::Scope::Workspace(cx.workspace.to_path_buf()),
            Self::User => ext::Scope::User,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ext::Configured;

    struct Settings(sys::config::Table);
    impl Table for Settings {
        fn name(&self) -> &str {
            self.0.name()
        }
        fn text(&self, key: &str) -> Result<Option<&str>, Refused> {
            self.0.text(key).map_err(|e| Refused(e.to_string()))
        }
        fn whole(&self, key: &str) -> Result<Option<u64>, Refused> {
            self.0.whole(key).map_err(|e| Refused(e.to_string()))
        }
        fn flag(&self, key: &str) -> Result<Option<bool>, Refused> {
            self.0.flag(key).map_err(|e| Refused(e.to_string()))
        }
        fn list(&self, key: &str) -> Result<Option<&[String]>, Refused> {
            self.0.list(key).map_err(|e| Refused(e.to_string()))
        }
    }
    fn configured(extra: &str) -> Result<crate::Mcp, Refused> {
        let config: sys::Config = format!("[ext.mcp]\nservers=[\"fixture\"]\n{extra}")
            .parse()
            .unwrap();
        crate::Mcp::configure(&Settings(config.ext.table("mcp")))
    }

    #[test]
    fn broader_stdio_scope_requires_explicit_experimental_consent_at_the_boundary() {
        for scope in ["workspace", "user"] {
            assert!(
                configured(&format!("fixture=[\"server\"]\nfixture_scope={scope:?}"))
                    .err()
                    .unwrap()
                    .0
                    .contains("experimental_stdio_sharing")
            );
            assert!(
                configured(&format!(
                    "fixture=[\"server\"]\nfixture_scope={scope:?}\nexperimental_stdio_sharing=true"
                ))
                .is_ok()
            );
        }
        let default = configured("fixture=[\"server\"]").unwrap();
        assert!(matches!(default.0.scopes[0], Scope::Session));
        let opted_in = configured("fixture=[\"server\"]\nexperimental_stdio_sharing=true").unwrap();
        assert!(matches!(opted_in.0.scopes[0], Scope::Session));
        assert!(configured("fixture=[\"server\"]\nfixture_scope=\"all\"").is_err());
    }

    #[test]
    fn http_has_no_client_controlled_remote_process_scope() {
        assert!(configured("fixture_url=\"http://127.0.0.1/mcp\"").is_ok());
        for scope in ["session", "workspace", "user"] {
            assert!(configured(&format!("fixture_url=\"http://127.0.0.1/mcp\"\nfixture_scope={scope:?}\nexperimental_stdio_sharing=true")).err().unwrap().0.contains("stdio processes only"));
        }
    }
}
