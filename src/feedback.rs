//! Feedback about signalbox itself, sent to its feedbackBox app: `signalbox
//! feedback` and the MCP `feedback` tool. The service URL is fixed and the
//! ingest key is baked in at build time (`SIGNALBOX_FEEDBACK_KEY`); both can be
//! overridden at runtime with `SIGNALBOX_FEEDBACK_URL` / `SIGNALBOX_FEEDBACK_KEY`.
//!
//! Each item carries a little metadata: signalbox's version and the machine
//! (always), and the device and app of the session running in this checkout,
//! if there is one. Never paths, logs or errors.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::io::IsTerminal;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const DEFAULT_URL: &str = "https://feedbackbox.hvalec.com";
const BAKED_KEY: Option<&str> = option_env!("SIGNALBOX_FEEDBACK_KEY");
const UA: &str = "signalbox-cli";
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_TITLE: usize = 200;
const MAX_DESCRIPTION: usize = 10_000;

/// Where feedback goes.
#[derive(Debug, PartialEq)]
pub struct Target {
    pub url: String,
    pub key: String,
}

/// The feedbackBox item created.
#[derive(Debug, PartialEq)]
pub struct Sent {
    pub number: u64,
    pub url: String,
}

#[derive(Clone, Copy)]
pub enum Source {
    Cli,
    Mcp,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Cli => "cli",
            Source::Mcp => "mcp",
        }
    }
}

pub fn target() -> Result<Target> {
    target_from(
        crate::rnconfig::env("FEEDBACK_URL"),
        crate::rnconfig::env("FEEDBACK_KEY"),
        BAKED_KEY,
    )
}

fn target_from(url: Option<String>, key: Option<String>, baked: Option<&str>) -> Result<Target> {
    let key = key
        .or_else(|| baked.map(String::from))
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "feedback isn't configured in this build (no feedbackBox ingest key). \
                 Set SIGNALBOX_FEEDBACK_KEY to send it."
            )
        })?;
    let url = url.unwrap_or_else(|| DEFAULT_URL.into());
    Ok(Target {
        url: url.trim().trim_end_matches('/').to_string(),
        key,
    })
}

fn endpoint(t: &Target) -> String {
    format!("{}/api/v1/feedback", t.url)
}

/// The request body, checked against the service's limits.
fn body(title: &str, description: &str, metadata: Value) -> Result<Value> {
    let title = title.trim();
    if title.is_empty() {
        bail!("feedback needs a title");
    }
    if title.chars().count() > MAX_TITLE {
        bail!("the title is too long (at most {MAX_TITLE} characters)");
    }
    let description = description.trim();
    if description.chars().count() > MAX_DESCRIPTION {
        bail!("the description is too long (at most {MAX_DESCRIPTION} characters)");
    }
    Ok(json!({ "title": title, "description": description, "metadata": metadata }))
}

fn parse_response(status: u16, text: &str) -> Result<Sent> {
    let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        let number = v["number"]
            .as_u64()
            .ok_or_else(|| anyhow!("feedbackBox answered {status} without an item number"))?;
        return Ok(Sent {
            number,
            url: v["url"].as_str().unwrap_or("").to_string(),
        });
    }
    match v["error"].as_str() {
        Some(e) => bail!("feedbackBox refused the feedback ({status}): {e}"),
        None => bail!("feedbackBox answered {status}"),
    }
}

fn post(t: &Target, body: &Value) -> Result<Sent> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(UA)
        .timeout(TIMEOUT)
        .build()?;
    let r = client
        .post(endpoint(t))
        .bearer_auth(&t.key)
        .json(body)
        .send()
        .with_context(|| format!("couldn't reach feedbackBox at {}", t.url))?;
    let status = r.status().as_u16();
    let text = r.text().unwrap_or_default();
    parse_response(status, &text)
}

/// Send one item, with metadata gathered for `source`.
pub fn send(title: &str, description: &str, source: Source) -> Result<Sent> {
    let t = target()?;
    let body = body(title, description, metadata(source))?;
    post(&t, &body)
}

// ---- metadata ----

fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

fn metadata(source: Source) -> Value {
    let mut m = Map::new();
    m.insert("tool".into(), json!("signalbox"));
    m.insert("version".into(), json!(crate::update::current_version()));
    m.insert(
        "channel".into(),
        json!(crate::update::current_channel().as_str()),
    );
    m.insert("os".into(), json!(std::env::consts::OS));
    m.insert("arch".into(), json!(std::env::consts::ARCH));
    if let Some(v) = output("sw_vers", &["-productVersion"]) {
        m.insert("macosVersion".into(), json!(v));
    }
    m.insert("source".into(), json!(source.as_str()));
    if let Some(s) = session_metadata() {
        m.extend(s);
    }
    Value::Object(m)
}

/// The device and app of the session running in this checkout, if any. Only
/// reads it (its session file and its `status`); nothing is started.
fn session_metadata() -> Option<Map<String, Value>> {
    let cwd = std::env::current_dir().ok()?;
    let (sock, sf) = crate::control::find_session(&cwd).ok()?;
    let status = crate::control::call(&sock, "status", json!({})).ok();
    let platform = sf.platform.as_deref();
    let os = sf.udid.as_deref().and_then(|udid| match platform {
        Some("ios_simulator") => crate::session::sim_runtime(udid).and_then(|r| runtime_os(&r)),
        Some("android") => output(
            &simon::android::find_bin("adb"),
            &["-s", udid, "shell", "getprop", "ro.build.version.release"],
        )
        .map(|v| format!("Android {v}")),
        _ => None,
    });
    let bundle = if platform == Some("android") {
        crate::rnconfig::load_rn_config()
            .ok()
            .flatten()
            .and_then(|c| crate::rnconfig::current_project(&c))
            .and_then(|p| p.android_bundle_id().map(String::from))
    } else {
        sf.bundle.clone()
    };
    let name = status
        .as_ref()
        .and_then(|s| s["device_name"].as_str().map(String::from))
        .or_else(|| match platform {
            Some("ios_simulator") => sf
                .udid
                .as_deref()
                .and_then(crate::session::sim_info)
                .map(|(_, n)| n),
            _ => None,
        });
    Some(session_fields(
        platform,
        name,
        os,
        bundle,
        rn_version(Path::new(&sf.root)),
        status.as_ref().and_then(|s| s["metro"].as_str()),
    ))
}

fn session_fields(
    platform: Option<&str>,
    device_name: Option<String>,
    device_os: Option<String>,
    bundle: Option<String>,
    rn: Option<String>,
    metro: Option<&str>,
) -> Map<String, Value> {
    let mut m = Map::new();
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            m.insert(k.into(), json!(v));
        }
    };
    put("platform", platform.map(String::from));
    put("deviceName", device_name);
    put("deviceOsVersion", device_os);
    put("bundleId", bundle);
    put("reactNativeVersion", rn);
    put("metroState", metro.map(String::from));
    m
}

/// `com.apple.CoreSimulator.SimRuntime.iOS-18-2` → `iOS 18.2`.
fn runtime_os(runtime: &str) -> Option<String> {
    let tail = runtime.rsplit('.').next()?;
    let (os, version) = tail.split_once('-')?;
    Some(format!("{os} {}", version.replace('-', ".")))
}

/// The project's installed react-native version.
fn rn_version(root: &Path) -> Option<String> {
    let raw = std::fs::read(root.join("node_modules/react-native/package.json")).ok()?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    v["version"].as_str().map(String::from)
}

// ---- `signalbox feedback` ----

pub fn command(title: Option<String>, message: Option<String>) {
    if let Err(e) = run_command(title, message) {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}

fn run_command(title: Option<String>, message: Option<String>) -> Result<()> {
    // Before asking anything: a build without a key can't send it anyway.
    target()?;
    let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let title = match title {
        Some(t) => t,
        None if tty => inquire::Text::new("Title:")
            .with_help_message("one line: the bug or idea, about signalbox itself")
            .with_validator(|s: &str| {
                Ok(if s.trim().is_empty() {
                    inquire::validator::Validation::Invalid("a title is needed".into())
                } else {
                    inquire::validator::Validation::Valid
                })
            })
            .prompt()?,
        None => bail!("no title: pass --title (and --message) when not in a terminal"),
    };
    let message = match message {
        Some(m) => m,
        None if tty => inquire::Text::new("Details (optional):")
            .prompt_skippable()?
            .unwrap_or_default(),
        None => String::new(),
    };
    let sent = send(&title, &message, Source::Cli)?;
    println!("Feedback sent (#{})", sent.number);
    if !sent.url.is_empty() {
        println!("{}", sent.url);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    #[test]
    fn target_prefers_env_and_needs_a_key() {
        let t = target_from(None, None, Some("fbi_baked")).unwrap();
        assert_eq!(
            t,
            Target {
                url: DEFAULT_URL.into(),
                key: "fbi_baked".into()
            }
        );
        let t = target_from(
            Some("http://localhost:3000/".into()),
            Some("fbi_env".into()),
            Some("fbi_baked"),
        )
        .unwrap();
        assert_eq!(t.url, "http://localhost:3000");
        assert_eq!(t.key, "fbi_env");
        assert_eq!(endpoint(&t), "http://localhost:3000/api/v1/feedback");
        let e = target_from(None, None, Some("")).unwrap_err().to_string();
        assert!(e.contains("isn't configured in this build"), "{e}");
    }

    #[test]
    fn body_is_checked() {
        let b = body(
            "  Crash on reload ",
            " details\n",
            json!({"tool": "signalbox"}),
        )
        .unwrap();
        assert_eq!(
            b,
            json!({"title": "Crash on reload", "description": "details", "metadata": {"tool": "signalbox"}})
        );
        assert!(body(" ", "", json!({})).is_err());
        assert!(body(&"x".repeat(201), "", json!({})).is_err());
        assert!(body(&"é".repeat(200), "", json!({})).is_ok());
        assert!(body("t", &"x".repeat(10_001), json!({})).is_err());
    }

    #[test]
    fn parses_responses() {
        let ok = parse_response(
            201,
            r#"{"id":"u1","number":7,"appSlug":"signalbox","url":"https://fb/apps/signalbox/feedback/7","createdAt":"2026-11-10T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(
            ok,
            Sent {
                number: 7,
                url: "https://fb/apps/signalbox/feedback/7".into()
            }
        );
        let e = parse_response(401, r#"{"error":"invalid ingest key"}"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("401") && e.contains("invalid ingest key"), "{e}");
        let e = parse_response(502, "<html>").unwrap_err().to_string();
        assert!(e.contains("502"), "{e}");
        assert!(parse_response(201, "{}").is_err());
    }

    #[test]
    fn session_fields_skip_unknowns() {
        let m = session_fields(
            Some("ios_simulator"),
            Some("iPhone 16".into()),
            runtime_os("com.apple.CoreSimulator.SimRuntime.iOS-18-2"),
            None,
            Some("0.76.5".into()),
            Some("running"),
        );
        assert_eq!(
            Value::Object(m),
            json!({"platform": "ios_simulator", "deviceName": "iPhone 16", "deviceOsVersion": "iOS 18.2",
                   "reactNativeVersion": "0.76.5", "metroState": "running"})
        );
    }

    #[test]
    fn metadata_has_no_paths() {
        let m = metadata(Source::Mcp);
        assert_eq!(m["tool"], "signalbox");
        assert_eq!(m["source"], "mcp");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".into());
        assert!(!m.to_string().contains(&home), "{m}");
    }

    #[test]
    fn reads_react_native_version() {
        let dir = std::env::temp_dir().join(format!("signalbox-fb-test-{}", std::process::id()));
        let pkg = dir.join("node_modules/react-native");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"react-native","version":"0.79.2"}"#,
        )
        .unwrap();
        assert_eq!(rn_version(&dir).as_deref(), Some("0.79.2"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(rn_version(&dir), None);
    }

    #[test]
    fn posts_to_feedbackbox() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = Vec::new();
            let mut len = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                head.push(line.trim_end().to_string());
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            let reply = r#"{"id":"u1","number":3,"appSlug":"signalbox","url":"http://fb/3","createdAt":"x"}"#;
            write!(
                &stream,
                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
            (head, String::from_utf8(body).unwrap())
        });
        let t = Target {
            url: format!("http://127.0.0.1:{port}"),
            key: "fbi_test".into(),
        };
        let sent = post(&t, &body("Hello", "", json!({"a": 1})).unwrap()).unwrap();
        assert_eq!(sent.number, 3);
        assert_eq!(sent.url, "http://fb/3");
        let (head, body) = server.join().unwrap();
        assert_eq!(head[0], "POST /api/v1/feedback HTTP/1.1");
        let has = |h: &str| head.iter().any(|l| l.eq_ignore_ascii_case(h));
        assert!(has("authorization: Bearer fbi_test"), "{head:?}");
        assert!(has("user-agent: signalbox-cli"), "{head:?}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v,
            json!({"title": "Hello", "description": "", "metadata": {"a": 1}})
        );
    }
}
