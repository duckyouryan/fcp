use fcp_client::{AutomationHandoff, ClientSet, ManualControlResult};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process;

fn main() {
    if let Err(error) = run() {
        eprintln!("fcpctl: {error}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut arguments: Vec<String> = env::args().skip(1).collect();
    if arguments.is_empty() || arguments.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(());
    }

    let mut fcp_socket = None;
    let mut fcptemp_socket = None;
    while !arguments.is_empty() {
        let option = arguments[0].clone();
        if option == "--fcp-socket" || option == "--fcptemp-socket" {
            if arguments.len() < 2 {
                return Err(format!("{option} requires a path"));
            }
            let path = PathBuf::from(arguments.remove(1));
            arguments.remove(0);
            if option == "--fcp-socket" { fcp_socket = Some(path); }
            else { fcptemp_socket = Some(path); }
        } else if let Some(path) = option.strip_prefix("--fcp-socket=") {
            fcp_socket = Some(PathBuf::from(path));
            arguments.remove(0);
        } else if let Some(path) = option.strip_prefix("--fcptemp-socket=") {
            fcptemp_socket = Some(PathBuf::from(path));
            arguments.remove(0);
        } else {
            break;
        }
    }
    if arguments.is_empty() {
        print_help();
        return Ok(());
    }

    let mut clients = ClientSet::from_env()?;
    if let Some(path) = fcp_socket { clients = clients.with_fcp_socket(path); }
    if let Some(path) = fcptemp_socket { clients = clients.with_fcptemp_socket(path); }

    match arguments[0].as_str() {
        "status" => cmd_status(&clients),
        "fans" => cmd_fans(&clients, &arguments[1..]),
        "fan" => cmd_fan(&clients, &arguments[1..]),
        "temp" => cmd_temp(&clients, &arguments[1..]),
        "help" => { print_help(); Ok(()) }
        other => Err(format!("unknown command '{other}'. Run 'fcpctl help'.")),
    }
}

fn cmd_status(clients: &ClientSet) -> Result<(), String> {
    let mut result = serde_json::Map::new();
    let mut failures = Vec::new();
    match clients.fcp_command("fans.list", json!({})) {
        Ok(data) => { result.insert("fcp".to_owned(), data); }
        Err(error) => {
            failures.push(format!("fcp: {error}"));
            result.insert("fcp_error".to_owned(), json!(error));
        }
    }
    match clients.fcptemp_command("status", json!({})) {
        Ok(data) => { result.insert("fcptemp".to_owned(), data); }
        Err(error) => {
            failures.push(format!("fcptemp: {error}"));
            result.insert("fcptemp_error".to_owned(), json!(error));
        }
    }
    print_json(&Value::Object(result))?;
    if failures.is_empty() { Ok(()) } else { Err(failures.join("; ")) }
}

fn cmd_fans(clients: &ClientSet, args: &[String]) -> Result<(), String> {
    if args.len() > 1 {
        return Err("usage: fcpctl fans [FAN]".to_owned());
    }
    let parameters = args.first().map(|label| json!({"label": label})).unwrap_or_else(|| json!({}));
    print_json(&clients.fcp_command("fans.list", parameters)?)
}

fn cmd_fan(clients: &ClientSet, args: &[String]) -> Result<(), String> {
    match args {
        [subcommand, label, value] if subcommand == "set" => {
            let throttle = parse_throttle(value)?;
            let result = clients.set_manual_throttle(label, throttle)?;
            report_handoff(&result);
            print_json(&result.data)
        }
        [subcommand, label, mode] if subcommand == "mode" => {
            validate_mode(mode)?;
            let result = clients.set_manual_mode(label, mode, None)?;
            report_handoff(&result);
            print_json(&result.data)
        }
        [subcommand, label, mode, throttle] if subcommand == "mode" => {
            validate_mode(mode)?;
            let throttle = parse_throttle(throttle)?;
            let result = clients.set_manual_mode(label, mode, Some(throttle))?;
            report_handoff(&result);
            print_json(&result.data)
        }
        [subcommand, label] if subcommand == "kickstart" => {
            print_json(&clients.fcp_command("fan.kickstart", json!({"label": label}))?)
        }
        _ => Err("usage: fcpctl fan set FAN THROTTLE | fan mode FAN low|high|percent [THROTTLE] | fan kickstart FAN".to_owned()),
    }
}

fn report_handoff(result: &ManualControlResult) {
    if let AutomationHandoff::ServiceUnavailable(error) = &result.automation {
        eprintln!("warning: fcptemp could not be queried before manual control ({error}); ensure temperature automation is not running through another socket");
    }
}

fn cmd_temp(clients: &ClientSet, args: &[String]) -> Result<(), String> {
    match args {
        [subcommand] if subcommand == "status" => print_json(&clients.fcptemp_command("status", json!({}))?),
        [subcommand] if subcommand == "sensors" => print_json(&clients.fcptemp_command("sensors.list", json!({}))?),
        [subcommand] if subcommand == "rules" => print_json(&clients.fcptemp_command("rules.list", json!({}))?),
        [subcommand] if subcommand == "enable" => print_json(&clients.fcptemp_command("control.set", json!({"enabled": true}))?),
        [subcommand] if subcommand == "disable" => print_json(&clients.fcptemp_command("control.set", json!({"enabled": false}))?),
        [group, action, file] if group == "rule" && action == "set" => {
            let rule = read_json_value(file)?;
            if !rule.is_object() {
                return Err("rule file must contain one JSON object".to_owned());
            }
            print_json(&clients.fcptemp_command("rule.set", json!({"rule": rule}))?)
        }
        [group, action, fan] if group == "rule" && action == "delete" => {
            print_json(&clients.fcptemp_command("rule.delete", json!({"fan": fan}))?)
        }
        [group, action, fan] if group == "rule" && action == "enable" => {
            print_json(&clients.fcptemp_command("control.set", json!({"fan": fan, "enabled": true}))?)
        }
        [group, action, fan] if group == "rule" && action == "disable" => {
            print_json(&clients.fcptemp_command("control.set", json!({"fan": fan, "enabled": false}))?)
        }
        _ => Err("usage: fcpctl temp status|sensors|rules|enable|disable | temp rule set FILE|- | temp rule delete FAN | temp rule enable FAN | temp rule disable FAN".to_owned()),
    }
}

fn read_json_value(path: &str) -> Result<Value, String> {
    let mut content = String::new();
    if path == "-" {
        io::stdin().read_to_string(&mut content).map_err(|error| format!("cannot read rule JSON from stdin: {error}"))?;
    } else {
        content = fs::read_to_string(path).map_err(|error| format!("cannot read rule file {path}: {error}"))?;
    }
    serde_json::from_str(&content).map_err(|error| format!("invalid rule JSON: {error}"))
}

fn parse_throttle(value: &str) -> Result<u8, String> {
    let throttle: u8 = value.parse().map_err(|_| format!("invalid throttle '{value}'; expected an integer from 0 to 100"))?;
    if throttle > 100 { return Err("throttle must be between 0 and 100".to_owned()); }
    Ok(throttle)
}

fn validate_mode(mode: &str) -> Result<(), String> {
    match mode {
        "low" | "high" | "percent" => Ok(()),
        _ => Err(format!("invalid mode '{mode}'; use low, high, or percent")),
    }
}

fn print_json(value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| format!("cannot format JSON output: {error}"))?;
    println!("{text}");
    Ok(())
}

fn print_help() {
    println!(r#"fcpctl — CLI for fcp and fcptemp

Usage:
  fcpctl [--fcp-socket PATH] [--fcptemp-socket PATH] COMMAND

General:
  status                         Status from both services
  fans [FAN]                     List fans and current RPM
  fan set FAN PERCENT              Set manual throttle (0..100)
  fan mode FAN MODE [PERCENT]      Set low, high or percent mode
  fan kickstart FAN                Retry fan control initialization

Temperature automation:
  temp status                     Automation status
  temp sensors                    List temperature sensors
  temp rules                      List configured rules and runtime state
  temp rule set FILE|-              Create/replace rule from JSON file or stdin
  temp rule delete FAN              Delete a fan's temperature rule
  temp rule enable FAN              Enable one rule (global automation must be on)
  temp rule disable FAN             Disable one fan's rule
  temp enable                     Enable automation globally
  temp disable                    Disable automation globally

Environment:
  FCP_SOCKET_PATH                 Override the fcp socket path
  FCPTEMP_SOCKET_PATH             Override the fcptemp socket path
  --fcp-socket PATH               Override the fcp socket for this invocation
  --fcptemp-socket PATH           Override the fcptemp socket for this invocation

All command results are printed as JSON. Manual fan commands try to disable
an enabled fcptemp rule for that fan before applying the requested fcp setting.
"#);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_boundary_throttles() {
        assert_eq!(parse_throttle("0").unwrap(), 0);
        assert_eq!(parse_throttle("100").unwrap(), 100);
    }

    #[test]
    fn rejects_throttle_outside_range() {
        assert!(parse_throttle("101").is_err());
        assert!(parse_throttle("-1").is_err());
        assert!(parse_throttle("abc").is_err());
    }

    #[test]
    fn mode_is_limited_to_api_modes() {
        assert!(validate_mode("percent").is_ok());
        assert!(validate_mode("auto").is_err());
    }
}
