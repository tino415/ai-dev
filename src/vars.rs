use crate::model::{VariableSet, Variables};

pub fn effective(vars: &Variables, profile: Option<&str>, notebook: &str) -> VariableSet {
    let mut set = profile
        .and_then(|name| vars.profiles.get(name))
        .cloned()
        .unwrap_or_default();
    if let Some(overrides) = vars.notebooks.get(notebook) {
        set.extend(overrides.clone());
    }
    set
}

pub fn render(template: &str, set: &VariableSet) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let name = after[..end].trim();
        let variable = set
            .get(name)
            .ok_or_else(|| format!("variable {name} is not defined"))?;
        out.push_str(&variable.value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

pub fn mask(text: &str, set: &VariableSet) -> String {
    let mut values: Vec<&str> = set
        .values()
        .filter(|variable| variable.secret && !variable.value.is_empty())
        .map(|variable| variable.value.as_str())
        .collect();
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values.into_iter().fold(text.to_string(), |masked, value| {
        masked.replace(value, "***")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Variable;

    #[test]
    fn a_secret_inside_a_longer_secret_does_not_leak_the_rest() {
        let masked = mask("abcdef", &set(&[("a", "abc", true), ("b", "abcdef", true)]));
        assert_eq!(masked, "***");
    }

    fn set(entries: &[(&str, &str, bool)]) -> VariableSet {
        entries
            .iter()
            .map(|(name, value, secret)| {
                (
                    name.to_string(),
                    Variable {
                        value: value.to_string(),
                        secret: *secret,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn renders_variables_and_reports_missing_ones() {
        let vars = set(&[("ns", "prod", false)]);
        assert_eq!(
            render("kubectl -n {{ ns }} get pods", &vars).unwrap(),
            "kubectl -n prod get pods"
        );
        assert!(render("{{nope}}", &vars).unwrap_err().contains("nope"));
    }

    #[test]
    fn masks_only_secret_values() {
        let vars = set(&[("pw", "hunter2", true), ("ns", "prod", false)]);
        assert_eq!(mask("pw hunter2 in prod", &vars), "pw *** in prod");
    }

    #[test]
    fn notebook_overrides_win_over_profile() {
        let mut vars = Variables::default();
        vars.profiles
            .insert("prod".into(), set(&[("ns", "prod", false)]));
        vars.notebooks
            .insert("nb".into(), set(&[("ns", "debug", false)]));
        assert_eq!(effective(&vars, Some("prod"), "nb")["ns"].value, "debug");
        assert_eq!(effective(&vars, Some("prod"), "other")["ns"].value, "prod");
    }
}
