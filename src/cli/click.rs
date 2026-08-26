//! `browser-control click` — Playwright-backed, model-friendly clicking.
//!
//! The command deliberately exposes semantic element targeting instead of
//! requiring callers to synthesize JavaScript. A human-readable element name
//! is enough when it uniquely identifies an interactive control; adding
//! `--role` mirrors the role/name pair shown by `browser_snapshot`. CSS and
//! Playwright selectors remain available as an explicit fallback.

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::cli::route::{self, Route};
use crate::cli::trace::CommandTrace;
use crate::detect::Engine;
use crate::session::backend::{open_backend, LiveTarget};
use crate::sidecar::{Sidecar, SidecarConfig};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    browser: Option<String>,
    element: Option<String>,
    role: Option<String>,
    selector: Option<String>,
    fuzzy: bool,
    double_click: bool,
    button: String,
    modifiers: Vec<String>,
    target: Option<String>,
    timeout_ms: u64,
    json_output: bool,
) -> Result<()> {
    let mut trace = CommandTrace::new("click");
    let result = run_inner(
        browser,
        element,
        role,
        selector,
        fuzzy,
        double_click,
        button,
        modifiers,
        target,
        timeout_ms,
        json_output,
        &mut trace,
    )
    .await;
    trace.finish(result)
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    browser: Option<String>,
    element: Option<String>,
    role: Option<String>,
    selector: Option<String>,
    fuzzy: bool,
    double_click: bool,
    button: String,
    modifiers: Vec<String>,
    target: Option<String>,
    timeout_ms: u64,
    json_output: bool,
    trace: &mut CommandTrace,
) -> Result<()> {
    let mut params = build_click_params(
        element,
        role,
        selector,
        fuzzy,
        double_click,
        button,
        modifiers,
        timeout_ms,
    )?;
    let route = route::preamble(browser, target.as_deref(), trace).await?;
    if route.resolved.engine != Engine::Cdp {
        bail!(
            "`browser-control click` requires a Chromium-family browser because Playwright \
             cannot attach to a user-launched Firefox session; select chrome, edge, chromium, \
             or brave"
        );
    }

    let target_id = resolve_click_target(&route, target.as_deref(), trace).await?;
    trace.target_id(&target_id);
    params.insert("target_id".into(), Value::String(target_id));

    let sidecar = Sidecar::start(SidecarConfig::default())
        .await
        .context("starting Playwright click runtime")?;
    sidecar
        .connect(&route.resolved.endpoint)
        .await
        .context("connecting Playwright click runtime to the browser")?;
    let result = sidecar
        .call("click", Value::Object(params))
        .await
        .context("click failed")?;

    println!("{}", format_click_output(&result, json_output));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_click_params(
    element: Option<String>,
    role: Option<String>,
    selector: Option<String>,
    fuzzy: bool,
    double_click: bool,
    button: String,
    modifiers: Vec<String>,
    timeout_ms: u64,
) -> Result<Map<String, Value>> {
    let element = element.filter(|value| !value.trim().is_empty());
    let role = role.filter(|value| !value.trim().is_empty());
    let selector = selector.filter(|value| !value.trim().is_empty());

    if selector.is_some() && (element.is_some() || role.is_some()) {
        bail!(
            "choose one click strategy: positional ELEMENT with optional `--role`, or `--selector`"
        );
    }
    if role.is_some() && element.is_none() {
        bail!("`--role` requires positional ELEMENT (the accessible name)");
    }
    if selector.is_none() && element.is_none() {
        bail!(
            "missing click target; pass visible text/accessibility name as ELEMENT, or use `--selector`"
        );
    }

    let mut params = Map::new();
    if let Some(value) = element {
        params.insert("element".into(), Value::String(value));
    }
    if let Some(value) = role {
        params.insert("role".into(), Value::String(value));
    }
    if let Some(value) = selector {
        params.insert("selector".into(), Value::String(value));
    }
    params.insert("exact".into(), Value::Bool(!fuzzy));
    params.insert("double_click".into(), Value::Bool(double_click));
    params.insert("button".into(), Value::String(button));
    if !modifiers.is_empty() {
        params.insert("modifiers".into(), json!(modifiers));
    }
    params.insert("timeout_ms".into(), json!(timeout_ms));
    Ok(params)
}

async fn resolve_click_target(
    route: &Route,
    target: Option<&str>,
    trace: &mut CommandTrace,
) -> Result<String> {
    match (route.tab_name.as_deref(), target) {
        (Some(name), None) => {
            trace.route("named-tab").tab_name(name);
            route::run_named_tab(
                route,
                name,
                "named tabs (`<browser>/<name>`) require a registered browser; external \
                 endpoints can't carry tab names",
                |_backend, target_id| async move { Ok(target_id) },
            )
            .await
        }
        (None, maybe_regex) => {
            let backend = open_backend(&route.resolved.endpoint, route.resolved.engine).await?;
            let targets = backend.live_targets().await?;
            if let Some(pattern) = maybe_regex {
                trace.route("target-regex");
                let regex = Regex::new(pattern)
                    .with_context(|| format!("invalid `--target` URL regex `{pattern}`"))?;
                choose_unique_target(
                    targets
                        .into_iter()
                        .filter(|candidate| regex.is_match(&candidate.url)),
                    &format!("URL regex `{pattern}`"),
                    "use a narrower `--target` regex",
                )
            } else {
                trace.route("active-page");
                choose_unique_target(
                    targets.into_iter(),
                    "the selected browser",
                    "use `-b <browser>/<named-tab>` or `--target <url-regex>`",
                )
            }
        }
        _ => unreachable!("tab/target mutual exclusion was checked by route::preamble"),
    }
}

fn choose_unique_target(
    candidates: impl Iterator<Item = LiveTarget>,
    description: &str,
    correction: &str,
) -> Result<String> {
    let candidates: Vec<LiveTarget> = candidates.collect();
    match candidates.as_slice() {
        [] => Err(anyhow!(
            "no live page matched {description}; open or select a page and retry"
        )),
        [only] => Ok(only.id.clone()),
        many => {
            let urls = many
                .iter()
                .take(5)
                .map(|candidate| candidate.url.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow!(
                "{description} matched {} live pages; refusing to guess ({urls}); {correction}",
                many.len()
            ))
        }
    }
}

fn format_click_output(value: &Value, json_output: bool) -> String {
    if json_output {
        return serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    }

    let strategy = value
        .get("strategy")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let description = match (
        value.get("role").and_then(Value::as_str),
        value.get("element").and_then(Value::as_str),
        value.get("selector").and_then(Value::as_str),
    ) {
        (Some(role), Some(element), _) => format!("{role} {element:?}"),
        (_, Some(element), _) => format!("{element:?}"),
        (_, _, Some(selector)) => format!("selector {selector:?}"),
        _ => "element".to_string(),
    };
    format!("clicked {description} ({strategy})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_click_builds_role_and_name() {
        let params = build_click_params(
            Some("Sign in".into()),
            Some("button".into()),
            None,
            false,
            false,
            "left".into(),
            vec![],
            10_000,
        )
        .unwrap();
        assert_eq!(params["element"], "Sign in");
        assert_eq!(params["role"], "button");
        assert_eq!(params["exact"], true);
        assert_eq!(params["button"], "left");
    }

    #[test]
    fn selector_and_semantic_target_conflict() {
        let err = build_click_params(
            Some("Save".into()),
            None,
            Some("#save".into()),
            false,
            false,
            "left".into(),
            vec![],
            10_000,
        )
        .unwrap_err();
        assert!(err.to_string().contains("choose one click strategy"));
    }

    #[test]
    fn role_requires_element() {
        let err = build_click_params(
            None,
            Some("button".into()),
            None,
            false,
            false,
            "left".into(),
            vec![],
            10_000,
        )
        .unwrap_err();
        assert!(err.to_string().contains("requires positional ELEMENT"));
    }

    #[test]
    fn multiple_pages_are_rejected() {
        let candidates = vec![
            LiveTarget {
                id: "A".into(),
                url: "https://one.example/".into(),
                title: "One".into(),
            },
            LiveTarget {
                id: "B".into(),
                url: "https://two.example/".into(),
                title: "Two".into(),
            },
        ];
        let err =
            choose_unique_target(candidates.into_iter(), "browser", "name a tab").unwrap_err();
        assert!(err.to_string().contains("refusing to guess"));
        assert!(err.to_string().contains("https://one.example/"));
    }

    #[test]
    fn human_output_reports_resolved_strategy() {
        let value = json!({
            "ok": true,
            "strategy": "role",
            "role": "button",
            "element": "Sign in"
        });
        assert_eq!(
            format_click_output(&value, false),
            "clicked button \"Sign in\" (role)"
        );
    }
}
