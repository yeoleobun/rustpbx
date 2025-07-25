use anyhow::{anyhow, Result};
use regex::Regex;
use rsipstack::{
    dialog::{authenticate::Credential, invitation::InviteOption},
    transport::SipAddr,
};
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
};
use tracing::{debug, info, warn};

use crate::{
    config::RouteResult,
    proxy::routing::{ActionType, DefaultRoute, RouteRule, RoutingState, TrunkConfig},
};

/// Main routing function
///
/// Routes INVITE requests based on configured routing rules and trunk configurations:
/// 1. Match routing rules by priority
/// 2. Apply rewrite rules
/// 3. Select target trunk
/// 4. Set destination, headers and credentials
pub async fn match_invite(
    trunks: Option<&HashMap<String, TrunkConfig>>,
    routes: Option<&Vec<RouteRule>>,
    default: Option<&DefaultRoute>,
    mut option: InviteOption,
    origin: &rsip::Request,
    routing_state: &RoutingState,
) -> Result<RouteResult> {
    let routes = match routes {
        Some(routes) => routes,
        None => return Ok(RouteResult::Forward(option)),
    };

    // Extract URI information early to avoid borrowing conflicts
    let caller_user = option.caller.user().unwrap_or_default().to_string();
    let caller_host = option.caller.host().clone();
    let callee_user = option.callee.user().unwrap_or_default().to_string();
    let callee_host = option.callee.host().clone();
    let request_user = origin.uri.user().unwrap_or_default().to_string();
    let request_host = origin.uri.host().clone();

    debug!(
        "Matching INVITE: caller={}@{}, callee={}@{}, request={}@{}",
        caller_user, caller_host, callee_user, callee_host, request_user, request_host
    );

    // Traverse routing rules by priority
    for rule in routes {
        match rule.disabled {
            Some(true) => continue,
            _ => (),
        }

        debug!("Evaluating rule: {}", rule.name);

        // Check matching conditions
        let rule_matched = matches_rule(
            rule,
            &origin,
            &caller_user,
            &caller_host,
            &callee_user,
            &callee_host,
            &request_user,
            &request_host,
        )?;

        if !rule_matched {
            continue;
        }

        info!("Matched rule: {}", rule.name);

        // Apply rewrite rules
        if let Some(rewrite) = &rule.rewrite {
            apply_rewrite_rules(&mut option, rewrite, origin)?;
        }

        // Handle based on action type
        match rule.action.get_action_type() {
            ActionType::Reject => {
                if let Some(reject_config) = &rule.action.reject {
                    let reason =
                        reject_config
                            .reason
                            .clone()
                            .unwrap_or_else(|| match reject_config.code {
                                403 => "Forbidden".to_string(),
                                404 => "Not Found".to_string(),
                                486 => "Busy Here".to_string(),
                                _ => "Rejected".to_string(),
                            });
                    info!(
                        "Rejecting call with code {} and reason: {}",
                        reject_config.code, reason
                    );
                    return Ok(RouteResult::Abort(reject_config.code, reason));
                } else {
                    return Ok(RouteResult::Abort(403, "Forbidden".to_string()));
                }
            }
            ActionType::Busy => {
                return Ok(RouteResult::Abort(486, "Busy Here".to_string()));
            }
            ActionType::Forward => {
                // Select trunk and apply configuration
                if let Some(dest_config) = &rule.action.dest {
                    let selected_trunk = select_trunk(
                        dest_config,
                        &rule.action.select,
                        &rule.action.hash_key,
                        &option,
                        routing_state,
                    )?;

                    if let Some(trunk_config) = trunks
                        .as_ref()
                        .and_then(|trunks| trunks.get(&selected_trunk))
                    {
                        apply_trunk_config(&mut option, trunk_config)?;
                        info!(
                            "Selected trunk: {} for destination: {}",
                            selected_trunk, trunk_config.dest
                        );
                        return Ok(RouteResult::Forward(option));
                    } else {
                        warn!("Trunk '{}' not found in configuration", selected_trunk);
                    }
                }
            }
        }
    }

    // If no rules matched, use default route
    debug!("No rules matched, using default route");

    let default = match default {
        Some(default) => default,
        None => return Ok(RouteResult::Forward(option)),
    };

    let selected_trunk = select_trunk(
        &default.dest,
        &default.select,
        &None,
        &option,
        routing_state,
    )?;

    if let Some(trunk_config) = trunks
        .as_ref()
        .and_then(|trunks| trunks.get(&selected_trunk))
    {
        apply_trunk_config(&mut option, trunk_config)?;
        info!(
            "Using default trunk: {} for destination: {}",
            selected_trunk, trunk_config.dest
        );
        return Ok(RouteResult::Forward(option));
    }

    // If nothing found, forward directly
    debug!("No trunk configuration found, forwarding directly");
    Ok(RouteResult::Forward(option))
}

/// Check if routing rule matches
fn matches_rule(
    rule: &crate::proxy::routing::RouteRule,
    origin: &rsip::Request,
    caller_user: &str,
    caller_host: &rsip::Host,
    callee_user: &str,
    callee_host: &rsip::Host,
    request_user: &str,
    request_host: &rsip::Host,
) -> Result<bool> {
    let conditions = &rule.match_conditions;

    // Check from.user
    if let Some(pattern) = &conditions.from_user {
        if !matches_pattern(pattern, caller_user)? {
            return Ok(false);
        }
    }

    // Check from.host
    if let Some(pattern) = &conditions.from_host {
        if !matches_pattern(pattern, &caller_host.to_string())? {
            return Ok(false);
        }
    }

    // Check to.user
    if let Some(pattern) = &conditions.to_user {
        if !matches_pattern(pattern, callee_user)? {
            return Ok(false);
        }
    }

    // Check to.host
    if let Some(pattern) = &conditions.to_host {
        if !matches_pattern(pattern, &callee_host.to_string())? {
            return Ok(false);
        }
    }

    // Check request_uri.user
    if let Some(pattern) = &conditions.request_uri_user {
        if !matches_pattern(pattern, request_user)? {
            return Ok(false);
        }
    }

    // Check request_uri.host
    if let Some(pattern) = &conditions.request_uri_host {
        if !matches_pattern(pattern, &request_host.to_string())? {
            return Ok(false);
        }
    }

    // Check compatibility fields
    if let Some(pattern) = &conditions.caller {
        let caller_full = format!("{}@{}", caller_user, caller_host);
        if !matches_pattern(pattern, &caller_full)? {
            return Ok(false);
        }
    }

    if let Some(pattern) = &conditions.callee {
        let callee_full = format!("{}@{}", callee_user, callee_host);
        if !matches_pattern(pattern, &callee_full)? {
            return Ok(false);
        }
    }

    // Check headers
    for (header_key, pattern) in &conditions.headers {
        if header_key.starts_with("header.") {
            let header_name = &header_key[7..]; // Remove "header." prefix
            if let Some(header_value) = get_header_value(origin, header_name) {
                if !matches_pattern(pattern, &header_value)? {
                    return Ok(false);
                }
            } else {
                return Ok(false); // header not exist
            }
        }
    }

    Ok(true)
}

/// Match pattern (supports regex)
fn matches_pattern(pattern: &str, value: &str) -> Result<bool> {
    // If pattern doesn't contain regex special characters, use exact match
    if !pattern.contains('^')
        && !pattern.contains('$')
        && !pattern.contains('*')
        && !pattern.contains('+')
        && !pattern.contains('?')
        && !pattern.contains('[')
        && !pattern.contains('(')
        && !pattern.contains('\\')
    {
        return Ok(pattern == value);
    }

    // Use regex matching
    let regex =
        Regex::new(pattern).map_err(|e| anyhow!("Invalid regex pattern '{}': {}", pattern, e))?;
    Ok(regex.is_match(value))
}

/// Get header value
fn get_header_value(request: &rsip::Request, header_name: &str) -> Option<String> {
    for header in request.headers.iter() {
        match header {
            rsip::Header::Other(name, value)
                if name.to_lowercase() == header_name.to_lowercase() =>
            {
                return Some(value.clone());
            }
            rsip::Header::UserAgent(value) if header_name.to_lowercase() == "user-agent" => {
                return Some(value.to_string());
            }
            rsip::Header::Contact(contact) if header_name.to_lowercase() == "contact" => {
                return Some(contact.to_string());
            }
            // Add other standard header handling
            _ => continue,
        }
    }
    None
}

/// Apply rewrite rules
fn apply_rewrite_rules(
    option: &mut InviteOption,
    rewrite: &crate::proxy::routing::RewriteRules,
    origin: &rsip::Request,
) -> Result<()> {
    // Rewrite caller
    if let Some(pattern) = &rewrite.from_user {
        let new_user =
            apply_rewrite_pattern_with_match(pattern, option.caller.user().unwrap_or_default())?;
        option.caller = update_uri_user(&option.caller, &new_user)?;
    }

    if let Some(pattern) = &rewrite.from_host {
        let new_host =
            apply_rewrite_pattern_with_match(pattern, &option.caller.host().to_string())?;
        option.caller = update_uri_host(&option.caller, &new_host)?;
    }

    // Rewrite callee
    if let Some(pattern) = &rewrite.to_user {
        let new_user =
            apply_rewrite_pattern_with_match(pattern, option.callee.user().unwrap_or_default())?;
        option.callee = update_uri_user(&option.callee, &new_user)?;
    }

    if let Some(pattern) = &rewrite.to_host {
        let new_host =
            apply_rewrite_pattern_with_match(pattern, &option.callee.host().to_string())?;
        option.callee = update_uri_host(&option.callee, &new_host)?;
    }

    // Rewrite compatibility fields
    if let Some(pattern) = &rewrite.caller {
        let caller_full = format!(
            "{}@{}",
            option.caller.user().unwrap_or_default(),
            option.caller.host()
        );
        let new_caller = apply_rewrite_pattern_with_match(pattern, &caller_full)?;
        if let Some((user, host)) = new_caller.split_once('@') {
            option.caller = update_uri_user(&option.caller, user)?;
            option.caller = update_uri_host(&option.caller, host)?;
        }
    }

    if let Some(pattern) = &rewrite.callee {
        let callee_full = format!(
            "{}@{}",
            option.callee.user().unwrap_or_default(),
            option.callee.host()
        );
        let new_callee = apply_rewrite_pattern_with_match(pattern, &callee_full)?;
        if let Some((user, host)) = new_callee.split_once('@') {
            option.callee = update_uri_user(&option.callee, user)?;
            option.callee = update_uri_host(&option.callee, host)?;
        }
    }

    // Add or modify headers
    for (header_key, pattern) in &rewrite.headers {
        if header_key.starts_with("header.") {
            let header_name = &header_key[7..];
            let new_value = apply_rewrite_pattern(pattern, "", origin)?;

            let new_header = rsip::Header::Other(header_name.to_string(), new_value);

            if option.headers.is_none() {
                option.headers = Some(Vec::new());
            }
            option.headers.as_mut().unwrap().push(new_header);
        }
    }

    Ok(())
}

/// Apply rewrite pattern (supports capture groups)
fn apply_rewrite_pattern_with_match(pattern: &str, original: &str) -> Result<String> {
    // Support simple replacement patterns like "0{1}" where {1} is capture group
    if pattern.contains('{') && pattern.contains('}') {
        // This is a pattern with capture groups, need to extract from original value
        // Simplified implementation: assume pattern is "prefix{1}suffix" format
        let start = pattern.find('{').unwrap();
        let end = pattern.find('}').unwrap();
        let prefix = &pattern[..start];
        let suffix = &pattern[end + 1..];
        let group_str = &pattern[start + 1..end];

        if let Ok(group_num) = group_str.parse::<usize>() {
            // For patterns like "0{1}", {1} represents first capture group
            // We need to find what regex pattern was used to match this value
            // and extract the corresponding capture group

            // Try common regex patterns to extract capture groups
            let capture_result = extract_capture_group(original, group_num);
            if let Some(captured) = capture_result {
                return Ok(format!("{}{}{}", prefix, captured, suffix));
            }

            // Fallback: More general handling: if there are digits, extract digit part
            let digits: String = original.chars().filter(|c| c.is_ascii_digit()).collect();
            return Ok(format!("{}{}{}", prefix, digits, suffix));
        }
    }

    // Direct replacement
    Ok(pattern.to_string())
}

/// Extract capture group from common patterns
fn extract_capture_group(original: &str, group_num: usize) -> Option<String> {
    // Common regex patterns we support
    let patterns = [
        // +86(1\d{10}) - Chinese mobile number
        (r"^\+86(1\d{10})$", vec![3]), // Group 1 starts at position 3
        // +1(\d{10}) - US number
        (r"^\+1(\d{10})$", vec![2]), // Group 1 starts at position 2
        // (\d+) - any digits
        (r"^(\d+)$", vec![0]), // Group 1 is the entire string if all digits
        // prefix(\d+)suffix
        (r"^[^\d]*(\d+)[^\d]*$", vec![]), // Will be computed dynamically
    ];

    for (pattern_str, positions) in &patterns {
        if let Ok(regex) = Regex::new(pattern_str) {
            if let Some(captures) = regex.captures(original) {
                if group_num <= captures.len() && group_num > 0 {
                    if let Some(capture) = captures.get(group_num) {
                        return Some(capture.as_str().to_string());
                    }
                }
                // Fallback for simple position-based extraction
                if !positions.is_empty() && group_num == 1 && positions.len() >= 1 {
                    let start_pos = positions[0];
                    if original.len() > start_pos {
                        // Extract digits from this position onward
                        let substr = &original[start_pos..];
                        let digits: String =
                            substr.chars().take_while(|c| c.is_ascii_digit()).collect();
                        if !digits.is_empty() {
                            return Some(digits);
                        }
                    }
                }
            }
        }
    }

    None
}

/// Apply rewrite pattern
fn apply_rewrite_pattern(pattern: &str, original: &str, _origin: &rsip::Request) -> Result<String> {
    // Support simple replacement patterns like "96123{1}" where {1} is capture group
    if pattern.contains('{') && pattern.contains('}') {
        // This is a pattern with capture groups, need to extract from original value
        // Simplified implementation: assume pattern is "prefix{1}suffix" format
        let start = pattern.find('{').unwrap();
        let end = pattern.find('}').unwrap();
        let prefix = &pattern[..start];
        let suffix = &pattern[end + 1..];
        let _group_num: usize = pattern[start + 1..end].parse().unwrap_or(1);

        // Should use previously matched capture groups here, simplified implementation returns original value
        Ok(format!("{}{}{}", prefix, original, suffix))
    } else {
        // Direct replacement
        Ok(pattern.to_string())
    }
}

/// Update URI user part
fn update_uri_user(uri: &rsip::Uri, new_user: &str) -> Result<rsip::Uri> {
    let mut new_uri = uri.clone();
    new_uri.auth = Some(rsip::Auth {
        user: new_user.to_string(),
        password: uri.auth.as_ref().and_then(|a| a.password.clone()),
    });
    Ok(new_uri)
}

/// Update URI host part
fn update_uri_host(uri: &rsip::Uri, new_host: &str) -> Result<rsip::Uri> {
    let mut new_uri = uri.clone();
    new_uri.host_with_port = new_host
        .try_into()
        .map_err(|e| anyhow!("Invalid host '{}': {:?}", new_host, e))?;
    Ok(new_uri)
}

/// Select trunk
fn select_trunk(
    dest_config: &crate::proxy::routing::DestConfig,
    select_method: &str,
    hash_key: &Option<String>,
    option: &InviteOption,
    routing_state: &RoutingState,
) -> Result<String> {
    let trunks = match dest_config {
        crate::proxy::routing::DestConfig::Single(trunk) => vec![trunk.clone()],
        crate::proxy::routing::DestConfig::Multiple(trunk_list) => trunk_list.clone(),
    };

    if trunks.is_empty() {
        return Err(anyhow!("No trunks configured"));
    }

    if trunks.len() == 1 {
        return Ok(trunks[0].clone());
    }

    match select_method {
        "random" => {
            use rand::Rng;
            let index = rand::rng().random_range(0..trunks.len());
            Ok(trunks[index].clone())
        }
        "hash" => {
            let hash_value = if let Some(key) = hash_key {
                match key.as_str() {
                    "from.user" => option.caller.user().unwrap_or_default().to_string(),
                    "to.user" => option.callee.user().unwrap_or_default().to_string(),
                    "call-id" => "default".to_string(), // Simplified implementation
                    _ => key.clone(),
                }
            } else {
                option.caller.to_string()
            };

            let mut hasher = DefaultHasher::new();
            hash_value.hash(&mut hasher);
            let index = (hasher.finish() as usize) % trunks.len();
            Ok(trunks[index].clone())
        }
        "rr" | _ => {
            // Real round-robin implementation with state
            let destination_key = format!("{:?}", dest_config);
            let index = routing_state.next_round_robin_index(&destination_key, trunks.len());
            Ok(trunks[index].clone())
        }
    }
}

/// Apply trunk configuration
fn apply_trunk_config(option: &mut InviteOption, trunk: &TrunkConfig) -> Result<()> {
    // Set destination
    let dest_uri: rsip::Uri = trunk
        .dest
        .as_str()
        .try_into()
        .map_err(|e| anyhow!("Invalid trunk destination '{}': {:?}", trunk.dest, e))?;

    let transport = if let Some(transport_str) = &trunk.transport {
        match transport_str.to_lowercase().as_str() {
            "udp" => Some(rsip::transport::Transport::Udp),
            "tcp" => Some(rsip::transport::Transport::Tcp),
            "tls" => Some(rsip::transport::Transport::Tls),
            "ws" => Some(rsip::transport::Transport::Ws),
            "wss" => Some(rsip::transport::Transport::Wss),
            _ => None,
        }
    } else {
        None
    };

    option.destination = Some(SipAddr {
        r#type: transport,
        addr: dest_uri.host_with_port.clone(),
    });

    // Set authentication info
    if let (Some(username), Some(password)) = (&trunk.username, &trunk.password) {
        option.credential = Some(Credential {
            username: username.clone(),
            password: password.clone(),
            realm: dest_uri.host().to_string().into(),
        });
    }

    // Add trunk related headers
    if option.headers.is_none() {
        option.headers = Some(Vec::new());
    }

    let headers = option.headers.as_mut().unwrap();

    // Add P-Asserted-Identity header
    if trunk.username.is_some() {
        let pai_header = rsip::Header::Other(
            "P-Asserted-Identity".to_string(),
            format!("<{}>", option.caller),
        );
        headers.push(pai_header);
    }

    Ok(())
}
