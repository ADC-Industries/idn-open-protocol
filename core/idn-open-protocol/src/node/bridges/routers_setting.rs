//! Routers the Node runs.

use super::*;

impl RoutersSetting {
    /// Changes one setting: `list` (`Name=SOURCE; …`, a source may be empty) or `route`
    /// (`Name=SOURCE` for a router already set; an empty source leaves it nowhere).
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "list" => {
                let mut l: Vec<(String, String)> = Vec::new();
                for item in value
                    .split([';', '\n'])
                    .map(str::trim)
                    .filter(|i| !i.is_empty())
                {
                    let (name, source) = Self::item(item)?;
                    if l.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name)) {
                        return Err(format!("routers.list: two routers named {name:?}"));
                    }
                    l.push((name, source));
                }
                if l.len() > ROUTERS_MAX {
                    return Err(format!("routers.list: at most {ROUTERS_MAX}"));
                }
                self.routers = l;
            }
            "route" => {
                let (name, source) = Self::item(value.trim())?;
                let r = self
                    .routers
                    .iter_mut()
                    .find(|(n, _)| n.eq_ignore_ascii_case(&name))
                    .ok_or_else(|| format!("routers.route: no router named {name:?}"))?;
                r.1 = source;
            }
            _ => return Err(format!("no routers setting {key:?}")),
        }
        Ok(())
    }

    /// `Name=SOURCE` (SOURCE may be empty) checked.
    pub(super) fn item(item: &str) -> Result<(String, String), String> {
        let (n, src) = item.split_once('=').unwrap_or((item, ""));
        let (name, src) = (n.trim(), src.trim());
        if name.is_empty()
            || name.chars().count() > 60
            || name.contains(['"', '(', ')', '=', ';', '|', '\0'])
            || name.chars().any(char::is_control)
            || option_like(name)
        {
            return Err(format!(
                "routers: {item:?}: a name of 1 to 60 characters, no quotes, brackets, = ; or |"
            ));
        }
        let source = if src.is_empty() {
            String::new()
        } else {
            aes67_source("routers", item, src)?
        };
        Ok((name.to_owned(), source))
    }

    /// `Name=SOURCE; …`.
    pub fn text(&self) -> String {
        self.routers
            .iter()
            .map(|(n, s)| format!("{n}={s}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// `<routers list/>`.
    pub fn to_element(&self) -> Element {
        Element::new("routers").with_attr("list", &self.text())
    }

    /// The setting of a `<routers>` element (a bad list is left empty).
    pub fn from_element(e: &Element) -> RoutersSetting {
        let mut r = RoutersSetting::default();
        if let Some(v) = e.attr("list") {
            let _ = r.set("list", v);
        }
        r
    }

    /// Its `idn router` command (`None` without routers): one process for them all, which
    /// takes the targets from the Node as they change, so switching one never restarts
    /// it (its receivers would stay where they were, §1.8).
    pub fn command(&self) -> Option<Vec<String>> {
        (!self.routers.is_empty()).then(|| vec!["router".into(), "--from-node".into()])
    }
}
