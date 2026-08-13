use crate::config::Config;

/// Returns the ordered list of backend names to try for the given request model.
///
/// Resolution order:
/// 1. `model` matches a chain name -> that chain's order.
/// 2. `model` matches a backend name -> a single-element chain.
/// 3. otherwise -> `server.default_chain` (itself a chain or a single backend).
pub fn resolve<'a>(model: &str, cfg: &'a Config) -> Result<Vec<&'a str>, String> {
    if let Some(chain) = cfg.chains.get(model) {
        return Ok(chain.order.iter().map(String::as_str).collect());
    }
    if let Some((name, _)) = cfg.backends.get_key_value(model) {
        return Ok(vec![name.as_str()]);
    }
    let dc = &cfg.server.default_chain;
    if let Some(chain) = cfg.chains.get(dc) {
        Ok(chain.order.iter().map(String::as_str).collect())
    } else if let Some((name, _)) = cfg.backends.get_key_value(dc) {
        Ok(vec![name.as_str()])
    } else {
        Err(format!("default_chain '{dc}' resolves to nothing"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;

    const C: &str = r#"
[server]
listen = "127.0.0.1:3456"
default_chain = "default"

[backends.a]
type = "cli"
bin = "/bin/echo"

[backends.b]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["a", "b"]
"#;

    #[test]
    fn resolves_chain_name() {
        let cfg = parse(C).unwrap();
        assert_eq!(resolve("default", &cfg).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn resolves_backend_name_as_singleton() {
        let cfg = parse(C).unwrap();
        assert_eq!(resolve("a", &cfg).unwrap(), vec!["a"]);
    }

    #[test]
    fn unknown_model_falls_back_to_default() {
        let cfg = parse(C).unwrap();
        assert_eq!(resolve("zzz", &cfg).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn empty_model_falls_back_to_default() {
        let cfg = parse(C).unwrap();
        assert_eq!(resolve("", &cfg).unwrap(), vec!["a", "b"]);
    }
}
