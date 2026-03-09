//! Native GitHub MCP tool dispatcher.
//!
//! Implements the same tool interface as the `mcp-github-router` WASM component
//! but runs natively in the operator process.  This avoids the complexity of
//! loading the WASM component through the pack runtime for the demo while
//! keeping the MCP tool contract identical.

use serde_json::{json, Value};

const GITHUB_API: &str = "https://api.github.com";

/// Fetch the authenticated user's login from GitHub API.
pub fn get_authenticated_user(token: &str) -> Result<String, String> {
    let data = github_get("/user", token)?;
    data["login"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "could not read login from /user".to_string())
}

/// Generate a dynamic GH-connected card with the correct owner.
pub fn build_connected_card(owner: &str) -> Value {
    json!({
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "type": "AdaptiveCard",
        "version": "1.3",
        "body": [
            {
                "type": "Container", "style": "good",
                "items": [{
                    "type": "ColumnSet",
                    "columns": [
                        {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": "\u{2705}", "size": "extraLarge"}], "verticalContentAlignment": "center"},
                        {"type": "Column", "width": "stretch", "items": [
                            {"type": "TextBlock", "text": format!("Connected as {owner}!"), "size": "large", "weight": "bolder", "wrap": true},
                            {"type": "TextBlock", "text": "What would you like to do?", "size": "small", "isSubtle": true, "wrap": true, "spacing": "none"}
                        ]}
                    ]
                }]
            },
            {"type": "TextBlock", "text": "Quick Actions", "size": "medium", "weight": "bolder", "spacing": "large"},
            {
                "type": "Container", "style": "accent", "spacing": "small",
                "selectAction": {"type": "Action.Submit", "data": {
                    "action": "mcp", "tool": "list_repos", "owner": "",
                    "args": "{\"owner\":\"\",\"per_page\":6}"
                }},
                "items": [{"type": "ColumnSet", "columns": [
                    {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": "\u{1f4c1}", "size": "large"}], "verticalContentAlignment": "center"},
                    {"type": "Column", "width": "stretch", "items": [
                        {"type": "TextBlock", "text": "My Repositories", "weight": "bolder"},
                        {"type": "TextBlock", "text": "Browse your recent repositories", "size": "small", "isSubtle": true, "spacing": "none", "wrap": true}
                    ]},
                    {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": "\u{25b6}"}], "verticalContentAlignment": "center"}
                ]}]
            },
            {
                "type": "Container", "style": "accent", "spacing": "small",
                "selectAction": {"type": "Action.Submit", "data": {
                    "action": "mcp", "tool": "search_my_prs", "owner": "",
                    "args": "{\"per_page\":8}"
                }},
                "items": [{"type": "ColumnSet", "columns": [
                    {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": "\u{1f500}", "size": "large"}], "verticalContentAlignment": "center"},
                    {"type": "Column", "width": "stretch", "items": [
                        {"type": "TextBlock", "text": "My Pull Requests", "weight": "bolder"},
                        {"type": "TextBlock", "text": "Open PRs you've authored across all repos", "size": "small", "isSubtle": true, "spacing": "none", "wrap": true}
                    ]},
                    {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": "\u{25b6}"}], "verticalContentAlignment": "center"}
                ]}]
            },
            {
                "type": "Container", "style": "accent", "spacing": "small",
                "selectAction": {"type": "Action.Submit", "data": {
                    "action": "mcp", "tool": "my_notifications", "owner": "",
                    "args": "{\"per_page\":8}"
                }},
                "items": [{"type": "ColumnSet", "columns": [
                    {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": "\u{1f514}", "size": "large"}], "verticalContentAlignment": "center"},
                    {"type": "Column", "width": "stretch", "items": [
                        {"type": "TextBlock", "text": "Notifications", "weight": "bolder"},
                        {"type": "TextBlock", "text": "Review requests, mentions, and updates", "size": "small", "isSubtle": true, "spacing": "none", "wrap": true}
                    ]},
                    {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": "\u{25b6}"}], "verticalContentAlignment": "center"}
                ]}]
            },
            {
                "type": "Container", "style": "accent", "spacing": "small",
                "selectAction": {"type": "Action.Submit", "data": {
                    "action": "mcp", "tool": "create_issue_form", "owner": "",
                    "args": "{\"per_page\":20}"
                }},
                "items": [{"type": "ColumnSet", "columns": [
                    {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": "\u{1f4dd}", "size": "large"}], "verticalContentAlignment": "center"},
                    {"type": "Column", "width": "stretch", "items": [
                        {"type": "TextBlock", "text": "Create Issue", "weight": "bolder"},
                        {"type": "TextBlock", "text": "File a new issue in any repository", "size": "small", "isSubtle": true, "spacing": "none", "wrap": true}
                    ]},
                    {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": "\u{25b6}"}], "verticalContentAlignment": "center"}
                ]}]
            },
            {
                "type": "Container", "style": "accent", "spacing": "small",
                "selectAction": {"type": "Action.Submit", "data": {
                    "action": "mcp", "tool": "my_recent_actions", "owner": "",
                    "args": "{\"per_page\":8}"
                }},
                "items": [{"type": "ColumnSet", "columns": [
                    {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": "\u{26a1}", "size": "large"}], "verticalContentAlignment": "center"},
                    {"type": "Column", "width": "stretch", "items": [
                        {"type": "TextBlock", "text": "CI/CD Runs", "weight": "bolder"},
                        {"type": "TextBlock", "text": "Recent GitHub Actions across your repos", "size": "small", "isSubtle": true, "spacing": "none", "wrap": true}
                    ]},
                    {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": "\u{25b6}"}], "verticalContentAlignment": "center"}
                ]}]
            },
            {
                "type": "Container", "style": "accent", "spacing": "small",
                "selectAction": {"type": "Action.Submit", "data": {
                    "action": "mcp", "tool": "search_my_issues", "owner": "",
                    "args": "{\"per_page\":8}"
                }},
                "items": [{"type": "ColumnSet", "columns": [
                    {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": "\u{1f41b}", "size": "large"}], "verticalContentAlignment": "center"},
                    {"type": "Column", "width": "stretch", "items": [
                        {"type": "TextBlock", "text": "My Issues", "weight": "bolder"},
                        {"type": "TextBlock", "text": "Issues assigned to you or you created", "size": "small", "isSubtle": true, "spacing": "none", "wrap": true}
                    ]},
                    {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": "\u{25b6}"}], "verticalContentAlignment": "center"}
                ]}]
            }
        ]
    })
}

/// Call a GitHub MCP tool by name.
pub fn call_tool(tool: &str, args: &Value, token: &str) -> Result<Value, String> {
    match tool {
        "list_repos" => list_repos(args, token),
        "list_pull_requests" => list_pull_requests(args, token),
        "search_my_prs" => search_my_prs(args, token),
        "my_notifications" => my_notifications(args, token),
        "create_issue" => create_issue(args, token),
        "create_issue_form" => create_issue_form(args, token),
        "list_workflow_runs" => list_workflow_runs(args, token),
        "my_recent_actions" => my_recent_actions(args, token),
        "search_my_issues" => search_my_issues(args, token),
        "repo_detail" => repo_detail(args, token),
        _ => Err(format!("unknown tool: {tool}")),
    }
}

fn github_get(path: &str, token: &str) -> Result<Value, String> {
    let url = format!("{GITHUB_API}{path}");
    eprintln!("[github_mcp] GET {url}");
    let mut resp = ureq::get(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "greentic-mcp-github/0.1")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| {
            eprintln!("[github_mcp] GET {path} FAILED: {e}");
            format!("HTTP GET {path}: {e}")
        })?;
    let body: Value = resp.body_mut().read_json()
        .map_err(|e| format!("JSON parse: {e}"))?;
    Ok(body)
}

fn github_post(path: &str, token: &str, payload: &Value) -> Result<Value, String> {
    let url = format!("{GITHUB_API}{path}");
    let payload_str = serde_json::to_string(payload)
        .map_err(|e| format!("serialize: {e}"))?;
    let mut resp = ureq::post(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("Content-Type", "application/json")
        .header("User-Agent", "greentic-mcp-github/0.1")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send(payload_str.as_bytes())
        .map_err(|e| format!("HTTP POST {path}: {e}"))?;
    let body: Value = resp.body_mut().read_json()
        .map_err(|e| format!("JSON parse: {e}"))?;
    Ok(body)
}

// ── Tools ──

fn list_repos(args: &Value, token: &str) -> Result<Value, String> {
    let owner = args["owner"].as_str().unwrap_or("");
    let sort = args["sort"].as_str().unwrap_or("updated");
    let per_page = args["per_page"].as_u64().unwrap_or(10);
    let page = args["page"].as_u64().unwrap_or(1);
    let repo_type = args["type"].as_str().unwrap_or("all");

    // If owner is empty or "me", use /user/repos (authenticated user's repos)
    let path = if owner.is_empty() || owner == "me" {
        format!("/user/repos?type={repo_type}&sort={sort}&per_page={per_page}&page={page}")
    } else {
        format!("/users/{owner}/repos?type={repo_type}&sort={sort}&per_page={per_page}&page={page}")
    };
    let data = github_get(&path, token)?;

    let repos: Vec<Value> = data
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|r| {
            json!({
                "full_name": r["full_name"],
                "description": r["description"],
                "language": r["language"],
                "stargazers_count": r["stargazers_count"],
                "open_issues_count": r["open_issues_count"],
                "updated_at": r["updated_at"],
                "html_url": r["html_url"],
                "private": r["private"],
            })
        })
        .collect();

    let has_more = repos.len() == per_page as usize;
    Ok(json!({ "repos": repos, "count": repos.len(), "page": page, "per_page": per_page, "has_more": has_more, "owner": owner }))
}

fn list_pull_requests(args: &Value, token: &str) -> Result<Value, String> {
    let owner = args["owner"].as_str().ok_or("owner required")?;
    let repo = args["repo"].as_str().ok_or("repo required")?;
    let state = args["state"].as_str().unwrap_or("open");
    let per_page = args["per_page"].as_u64().unwrap_or(10);
    let page = args["page"].as_u64().unwrap_or(1);

    let path = format!(
        "/repos/{owner}/{repo}/pulls?state={state}&per_page={per_page}&page={page}"
    );
    let data = github_get(&path, token)?;

    let prs: Vec<Value> = data
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|pr| {
            json!({
                "number": pr["number"],
                "title": pr["title"],
                "state": pr["state"],
                "user": pr["user"]["login"],
                "created_at": pr["created_at"],
                "updated_at": pr["updated_at"],
                "html_url": pr["html_url"],
                "draft": pr["draft"],
            })
        })
        .collect();

    Ok(json!({ "pull_requests": prs, "count": prs.len() }))
}

/// Fetch user's repos to build a dynamic create-issue form.
/// Fetch recent GitHub Actions runs across the user's repos.
fn my_recent_actions(args: &Value, token: &str) -> Result<Value, String> {
    let per_page = args["per_page"].as_u64().unwrap_or(8);
    // Get user's 5 most recently pushed repos, then fetch runs from each
    let repos_data = github_get("/user/repos?sort=pushed&per_page=5", token)?;
    let repos = repos_data.as_array().cloned().unwrap_or_default();

    let mut all_runs: Vec<Value> = Vec::new();
    for repo in &repos {
        let full_name = repo["full_name"].as_str().unwrap_or("");
        if full_name.is_empty() { continue; }
        let path = format!("/repos/{full_name}/actions/runs?per_page=3");
        if let Ok(data) = github_get(&path, token) {
            if let Some(runs) = data["workflow_runs"].as_array() {
                for run in runs {
                    all_runs.push(json!({
                        "id": run["id"],
                        "name": run["name"],
                        "status": run["status"],
                        "conclusion": run["conclusion"],
                        "head_branch": run["head_branch"],
                        "event": run["event"],
                        "created_at": run["created_at"],
                        "html_url": run["html_url"],
                        "run_number": run["run_number"],
                        "repo": full_name,
                        "display_title": run["display_title"],
                        "head_sha": run["head_sha"],
                    }));
                }
            }
        }
    }
    // Sort by created_at descending, take top N
    all_runs.sort_by(|a, b| {
        let ta = a["created_at"].as_str().unwrap_or("");
        let tb = b["created_at"].as_str().unwrap_or("");
        tb.cmp(ta)
    });
    all_runs.truncate(per_page as usize);

    Ok(json!({
        "workflow_runs": all_runs,
        "count": all_runs.len(),
    }))
}

/// Get details for a specific repo — shows sub-actions (PRs, issues, actions).
fn repo_detail(args: &Value, token: &str) -> Result<Value, String> {
    let full_name = args["full_name"].as_str().ok_or("full_name required")?;
    let data = github_get(&format!("/repos/{full_name}"), token)?;
    Ok(json!({
        "full_name": data["full_name"],
        "description": data["description"],
        "language": data["language"],
        "stargazers_count": data["stargazers_count"],
        "open_issues_count": data["open_issues_count"],
        "forks_count": data["forks_count"],
        "default_branch": data["default_branch"],
        "private": data["private"],
        "html_url": data["html_url"],
        "owner": data["owner"]["login"],
        "name": data["name"],
    }))
}

fn create_issue_form(args: &Value, token: &str) -> Result<Value, String> {
    let per_page = args["per_page"].as_u64().unwrap_or(20);
    let path = format!("/user/repos?sort=updated&per_page={per_page}");
    let data = github_get(&path, token)?;

    let repos: Vec<Value> = data
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|r| {
            let full_name = r["full_name"].as_str()?;
            let permissions = &r["permissions"];
            // Only include repos the user can push to (can create issues)
            let can_push = permissions["push"].as_bool().unwrap_or(false)
                || permissions["admin"].as_bool().unwrap_or(false);
            if can_push {
                Some(json!({
                    "full_name": full_name,
                    "name": r["name"],
                    "owner": r["owner"]["login"],
                    "private": r["private"],
                }))
            } else {
                None
            }
        })
        .collect();

    Ok(json!({ "repos": repos }))
}

fn create_issue(args: &Value, token: &str) -> Result<Value, String> {
    let owner = args["owner"].as_str().ok_or("owner required")?;
    let repo = args["repo"].as_str().ok_or("repo required")?;
    let title = args["title"].as_str().ok_or("title required")?;

    let mut body = json!({ "title": title });
    if let Some(b) = args["body"].as_str() {
        body["body"] = json!(b);
    }
    if let Some(labels) = args["labels"].as_array() {
        body["labels"] = json!(labels);
    }

    let path = format!("/repos/{owner}/{repo}/issues");
    let issue = github_post(&path, token, &body)?;

    Ok(json!({
        "number": issue["number"],
        "title": issue["title"],
        "state": issue["state"],
        "html_url": issue["html_url"],
        "created_at": issue["created_at"],
    }))
}

fn list_workflow_runs(args: &Value, token: &str) -> Result<Value, String> {
    let owner = args["owner"].as_str().ok_or("owner required")?;
    let repo = args["repo"].as_str().ok_or("repo required")?;
    let per_page = args["per_page"].as_u64().unwrap_or(10);
    let page = args["page"].as_u64().unwrap_or(1);

    let mut path = format!(
        "/repos/{owner}/{repo}/actions/runs?per_page={per_page}&page={page}"
    );
    if let Some(status) = args["status"].as_str() {
        path.push_str(&format!("&status={status}"));
    }

    let data = github_get(&path, token)?;

    let runs: Vec<Value> = data["workflow_runs"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|run| {
            json!({
                "id": run["id"],
                "name": run["name"],
                "status": run["status"],
                "conclusion": run["conclusion"],
                "head_branch": run["head_branch"],
                "event": run["event"],
                "created_at": run["created_at"],
                "html_url": run["html_url"],
                "run_number": run["run_number"],
                "repo": run["repository"]["full_name"],
                "display_title": run["display_title"],
                "head_sha": run["head_sha"],
            })
        })
        .collect();

    Ok(json!({
        "workflow_runs": runs,
        "total_count": data["total_count"],
        "count": runs.len(),
    }))
}

/// Search open PRs authored by the authenticated user across all repos.
fn search_my_prs(_args: &Value, token: &str) -> Result<Value, String> {
    let user = get_authenticated_user(token)?;
    let per_page = _args["per_page"].as_u64().unwrap_or(8);
    let q = format!("type:pr author:{user} is:open");
    let encoded_q = q.replace(' ', "+");
    let path = format!(
        "/search/issues?q={encoded_q}&per_page={per_page}&sort=updated&order=desc"
    );
    eprintln!("[github_mcp] search_my_prs path={path}");
    let data = github_get(&path, token)?;
    eprintln!("[github_mcp] search_my_prs total_count={}", data["total_count"]);

    let prs: Vec<Value> = data["items"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|pr| {
            let repo_url = pr["repository_url"].as_str().unwrap_or("");
            let repo_name = repo_url.rsplit('/').next().unwrap_or("?");
            json!({
                "number": pr["number"],
                "title": pr["title"],
                "state": pr["state"],
                "repo": repo_name,
                "created_at": pr["created_at"],
                "updated_at": pr["updated_at"],
                "html_url": pr["html_url"],
                "draft": pr["draft"],
            })
        })
        .collect();

    Ok(json!({ "pull_requests": prs, "count": prs.len(), "user": user }))
}

/// Fetch notifications for the authenticated user.
/// Search open issues involving the authenticated user (authored, assigned, mentioned).
fn search_my_issues(args: &Value, token: &str) -> Result<Value, String> {
    let user = get_authenticated_user(token)?;
    let per_page = args["per_page"].as_u64().unwrap_or(8);
    let filter = args["filter"].as_str().unwrap_or("involves");
    let query = match filter {
        "authored" => format!("type:issue author:{user} is:open"),
        "assigned" => format!("type:issue assignee:{user} is:open"),
        _ => format!("type:issue involves:{user} is:open"),
    };
    let encoded_q = query.replace(' ', "+");
    let path = format!(
        "/search/issues?q={encoded_q}&per_page={per_page}&sort=updated&order=desc"
    );
    let data = github_get(&path, token)?;

    let issues: Vec<Value> = data["items"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|issue| {
            let repo_url = issue["repository_url"].as_str().unwrap_or("");
            let repo_name = repo_url.rsplit('/').next().unwrap_or("?");
            let labels: Vec<String> = issue["labels"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|l| l["name"].as_str().map(|s| s.to_string()))
                .collect();
            json!({
                "number": issue["number"],
                "title": issue["title"],
                "state": issue["state"],
                "repo": repo_name,
                "user": issue["user"]["login"],
                "created_at": issue["created_at"],
                "updated_at": issue["updated_at"],
                "html_url": issue["html_url"],
                "labels": labels,
                "comments": issue["comments"],
            })
        })
        .collect();

    Ok(json!({ "issues": issues, "count": issues.len(), "user": user, "filter": filter }))
}

fn my_notifications(_args: &Value, token: &str) -> Result<Value, String> {
    let per_page = _args["per_page"].as_u64().unwrap_or(8);
    let path = format!("/notifications?per_page={per_page}&all=false");
    let data = github_get(&path, token)?;

    let notifs: Vec<Value> = data
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|n| {
            let repo = n["repository"]["full_name"].as_str().unwrap_or("?");
            let reason = n["reason"].as_str().unwrap_or("?");
            let title = n["subject"]["title"].as_str().unwrap_or("?");
            let ntype = n["subject"]["type"].as_str().unwrap_or("?");
            json!({
                "repo": repo,
                "reason": reason,
                "title": title,
                "type": ntype,
                "updated_at": n["updated_at"],
                "unread": n["unread"],
            })
        })
        .collect();

    Ok(json!({ "notifications": notifs, "count": notifs.len() }))
}

// ── Dynamic Adaptive Card generation ──

/// Generate an Adaptive Card JSON from MCP tool results.
pub fn render_card(tool: &str, result: &Value, owner: &str) -> Value {
    match tool {
        "list_repos" => render_repos_card(result, owner),
        "list_pull_requests" | "search_my_prs" => render_my_prs_card(result),
        "my_notifications" => render_notifications_card(result),
        "create_issue" => render_issue_created_card(result),
        "create_issue_form" => render_create_issue_form(result),
        "search_my_issues" => render_my_issues_card(result),
        "list_workflow_runs" | "my_recent_actions" => render_actions_card(result),
        "repo_detail" => render_repo_detail_card(result),
        _ => json!({
            "type": "AdaptiveCard",
            "version": "1.3",
            "body": [{"type": "TextBlock", "text": "Unknown tool result", "wrap": true}]
        }),
    }
}

fn render_repos_card(data: &Value, owner: &str) -> Value {
    let repos = data["repos"].as_array().cloned().unwrap_or_default();
    let page = data["page"].as_u64().unwrap_or(1);
    let per_page = data["per_page"].as_u64().unwrap_or(6);
    let has_more = data["has_more"].as_bool().unwrap_or(false);
    let data_owner = data["owner"].as_str().unwrap_or(owner);
    let title = if data_owner.is_empty() {
        "\u{1f4c1} My Repositories".to_string()
    } else {
        format!("\u{1f4c1} Repositories — {data_owner}")
    };
    let mut body: Vec<Value> = vec![json!({
        "type": "Container",
        "style": "emphasis",
        "items": [
            {"type": "TextBlock", "text": title, "size": "large", "weight": "bolder", "wrap": true},
            {"type": "TextBlock", "text": format!("Page {} \u{2022} {} repositories", page, repos.len()), "size": "small", "isSubtle": true, "wrap": true, "spacing": "none"}
        ]
    })];

    for repo in &repos {
        let name = repo["full_name"].as_str().unwrap_or("?");
        let desc = repo["description"].as_str().unwrap_or("No description");
        let lang = repo["language"].as_str().unwrap_or("\u{2014}");
        let stars = repo["stargazers_count"].as_u64().unwrap_or(0);
        let private = repo["private"].as_bool().unwrap_or(false);
        let icon = if private { "\u{1f512}" } else { "\u{1f4e6}" };

        body.push(json!({
            "type": "Container",
            "style": "accent",
            "spacing": "small",
            "selectAction": {"type": "Action.Submit", "data": {
                "action": "mcp", "tool": "repo_detail", "owner": "",
                "args": format!("{{\"full_name\":\"{name}\"}}")
            }},
            "items": [
                {"type": "TextBlock", "text": format!("{icon} {name}"), "weight": "bolder", "wrap": true},
                {"type": "TextBlock", "text": desc, "size": "small", "isSubtle": true, "wrap": true, "spacing": "none"},
                {"type": "ColumnSet", "columns": [
                    {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": format!("\u{2b50} {stars}"), "size": "small"}]},
                    {"type": "Column", "width": "auto", "items": [{"type": "TextBlock", "text": lang, "size": "small", "isSubtle": true}]},
                    {"type": "Column", "width": "stretch", "items": [{"type": "TextBlock", "text": "Tap for details \u{25b6}", "size": "small", "isSubtle": true, "horizontalAlignment": "right"}]}
                ]}
            ]
        }));
    }

    let mut actions: Vec<Value> = Vec::new();
    if page > 1 {
        actions.push(json!({
            "type": "Action.Submit", "title": "\u{2b05} Previous",
            "data": {"action": "mcp", "tool": "list_repos", "owner": data_owner,
                "args": format!("{{\"owner\":\"{data_owner}\",\"per_page\":{per_page},\"page\":{}}}", page - 1)}
        }));
    }
    if has_more {
        actions.push(json!({
            "type": "Action.Submit", "title": "Next \u{27a1}",
            "data": {"action": "mcp", "tool": "list_repos", "owner": data_owner,
                "args": format!("{{\"owner\":\"{data_owner}\",\"per_page\":{per_page},\"page\":{}}}", page + 1)}
        }));
    }
    actions.push(json!({"type": "Action.Submit", "title": "\u{2190} Back to Menu", "data": {"routeToCardId": "GH-connected", "step": "back"}}));

    json!({
        "type": "AdaptiveCard",
        "version": "1.3",
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "body": body,
        "actions": actions
    })
}

fn render_repo_detail_card(data: &Value) -> Value {
    let full_name = data["full_name"].as_str().unwrap_or("?");
    let desc = data["description"].as_str().unwrap_or("No description");
    let lang = data["language"].as_str().unwrap_or("\u{2014}");
    let stars = data["stargazers_count"].as_u64().unwrap_or(0);
    let issues = data["open_issues_count"].as_u64().unwrap_or(0);
    let forks = data["forks_count"].as_u64().unwrap_or(0);
    let branch = data["default_branch"].as_str().unwrap_or("main");
    let private = data["private"].as_bool().unwrap_or(false);
    let owner = data["owner"].as_str().unwrap_or("?");
    let name = data["name"].as_str().unwrap_or("?");
    let icon = if private { "\u{1f512}" } else { "\u{1f4e6}" };

    json!({
        "type": "AdaptiveCard",
        "version": "1.3",
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "body": [
            {
                "type": "Container", "style": "emphasis",
                "items": [
                    {"type": "TextBlock", "text": format!("{icon} {full_name}"), "size": "large", "weight": "bolder", "wrap": true},
                    {"type": "TextBlock", "text": desc, "size": "small", "isSubtle": true, "wrap": true, "spacing": "none"}
                ]
            },
            {
                "type": "FactSet", "spacing": "medium",
                "facts": [
                    {"title": "Language", "value": lang},
                    {"title": "Stars", "value": format!("\u{2b50} {stars}")},
                    {"title": "Open Issues", "value": format!("{issues}")},
                    {"title": "Forks", "value": format!("{forks}")},
                    {"title": "Default Branch", "value": branch},
                ]
            },
            {"type": "TextBlock", "text": "Actions", "size": "medium", "weight": "bolder", "spacing": "large"},
            {
                "type": "Container", "style": "accent", "spacing": "small",
                "selectAction": {"type": "Action.Submit", "data": {
                    "action": "mcp", "tool": "list_pull_requests", "owner": owner,
                    "args": format!("{{\"owner\":\"{owner}\",\"repo\":\"{name}\",\"state\":\"open\",\"per_page\":8}}")
                }},
                "items": [{"type": "TextBlock", "text": "\u{1f500} View Pull Requests", "weight": "bolder"}]
            },
            {
                "type": "Container", "style": "accent", "spacing": "small",
                "selectAction": {"type": "Action.Submit", "data": {
                    "action": "mcp", "tool": "list_workflow_runs", "owner": owner,
                    "args": format!("{{\"owner\":\"{owner}\",\"repo\":\"{name}\",\"per_page\":5}}")
                }},
                "items": [{"type": "TextBlock", "text": "\u{26a1} View CI/CD Runs", "weight": "bolder"}]
            },
            {
                "type": "Container", "style": "accent", "spacing": "small",
                "selectAction": {"type": "Action.Submit", "data": {
                    "action": "mcp", "tool": "create_issue_form", "owner": "",
                    "args": "{\"per_page\":20}"
                }},
                "items": [{"type": "TextBlock", "text": "\u{1f4dd} Create Issue", "weight": "bolder"}]
            }
        ],
        "actions": [
            {"type": "Action.Submit", "title": "\u{2190} Back to Repos", "data": {
                "action": "mcp", "tool": "list_repos", "owner": "",
                "args": "{\"owner\":\"\",\"per_page\":6}"
            }},
            {"type": "Action.Submit", "title": "\u{2190} Back to Menu", "data": {"routeToCardId": "GH-connected", "step": "back"}}
        ]
    })
}

fn render_prs_card(data: &Value) -> Value {
    let prs = data["pull_requests"].as_array().cloned().unwrap_or_default();
    let mut body: Vec<Value> = vec![json!({
        "type": "Container",
        "style": "emphasis",
        "items": [
            {"type": "TextBlock", "text": "\u{1f500} Pull Requests", "size": "large", "weight": "bolder", "wrap": true},
            {"type": "TextBlock", "text": format!("{} open PRs", prs.len()), "size": "small", "isSubtle": true, "spacing": "none"}
        ]
    })];

    for pr in &prs {
        let number = pr["number"].as_u64().unwrap_or(0);
        let title = pr["title"].as_str().unwrap_or("?");
        let user = pr["user"].as_str().unwrap_or("?");
        let draft = pr["draft"].as_bool().unwrap_or(false);
        let icon = if draft { "\u{1f4dd}" } else { "\u{1f7e2}" };

        body.push(json!({
            "type": "Container",
            "style": "accent",
            "spacing": "small",
            "items": [
                {"type": "TextBlock", "text": format!("{icon} #{number} {title}"), "weight": "bolder", "wrap": true},
                {"type": "TextBlock", "text": format!("by {user}"), "size": "small", "isSubtle": true, "spacing": "none"}
            ]
        }));
    }

    json!({
        "type": "AdaptiveCard",
        "version": "1.3",
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "body": body,
        "actions": [
            {"type": "Action.Submit", "title": "\u{2190} Back to Menu", "data": {"routeToCardId": "GH-connected", "step": "back"}}
        ]
    })
}

fn render_create_issue_form(data: &Value) -> Value {
    let repos = data["repos"].as_array().cloned().unwrap_or_default();
    let choices: Vec<Value> = repos
        .iter()
        .map(|r| {
            let full_name = r["full_name"].as_str().unwrap_or("?");
            let name = r["name"].as_str().unwrap_or("?");
            let private = r["private"].as_bool().unwrap_or(false);
            let icon = if private { "\u{1f512}" } else { "\u{1f4e6}" };
            json!({
                "title": format!("{icon} {full_name}"),
                "value": format!("{}:{}", r["owner"].as_str().unwrap_or("?"), name),
            })
        })
        .collect();

    json!({
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "type": "AdaptiveCard",
        "version": "1.3",
        "body": [
            {
                "type": "Container", "style": "emphasis",
                "items": [
                    {"type": "TextBlock", "text": "\u{1f4dd} Create New Issue", "size": "large", "weight": "bolder", "wrap": true},
                    {"type": "TextBlock", "text": format!("Choose from {} repositories you have access to", choices.len()), "size": "small", "isSubtle": true, "wrap": true, "spacing": "none"}
                ]
            },
            {
                "type": "Container", "spacing": "large",
                "items": [
                    {"type": "TextBlock", "text": "Repository", "size": "small", "weight": "bolder"},
                    {"type": "Input.ChoiceSet", "id": "repo_choice", "style": "filtered", "choices": choices}
                ]
            },
            {
                "type": "Container", "spacing": "medium",
                "items": [
                    {"type": "TextBlock", "text": "Title", "size": "small", "weight": "bolder"},
                    {"type": "Input.Text", "id": "issueTitle", "placeholder": "Brief description of the issue"}
                ]
            },
            {
                "type": "Container", "spacing": "medium",
                "items": [
                    {"type": "TextBlock", "text": "Description", "size": "small", "weight": "bolder"},
                    {"type": "Input.Text", "id": "issueBody", "placeholder": "Detailed description...", "isMultiline": true, "maxLength": 2000}
                ]
            },
            {
                "type": "Container", "spacing": "medium",
                "items": [
                    {"type": "TextBlock", "text": "Labels", "size": "small", "weight": "bolder"},
                    {"type": "Input.ChoiceSet", "id": "labels", "style": "filtered", "isMultiSelect": true, "choices": [
                        {"title": "\u{1f41b} bug", "value": "bug"},
                        {"title": "\u{2728} enhancement", "value": "enhancement"},
                        {"title": "\u{1f4d6} documentation", "value": "documentation"},
                        {"title": "\u{1f680} feature", "value": "feature"},
                        {"title": "\u{1f6a8} critical", "value": "critical"},
                        {"title": "\u{1f914} question", "value": "question"}
                    ]}
                ]
            }
        ],
        "actions": [
            {
                "type": "Action.Submit",
                "title": "\u{2705} Create Issue",
                "style": "positive",
                "data": {"action": "mcp", "tool": "create_issue"}
            },
            {
                "type": "Action.Submit",
                "title": "\u{2190} Back to Menu",
                "data": {"routeToCardId": "GH-connected", "step": "back"}
            }
        ]
    })
}

fn render_issue_created_card(data: &Value) -> Value {
    let number = data["number"].as_u64().unwrap_or(0);
    let title = data["title"].as_str().unwrap_or("?");
    let url = data["html_url"].as_str().unwrap_or("#");

    json!({
        "type": "AdaptiveCard",
        "version": "1.3",
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "body": [
            {"type": "Container", "style": "good", "items": [
                {"type": "TextBlock", "text": "\u{2705} Issue Created!", "size": "large", "weight": "bolder"},
                {"type": "TextBlock", "text": format!("#{number} — {title}"), "wrap": true, "spacing": "none"}
            ]},
            {"type": "FactSet", "spacing": "medium", "facts": [
                {"title": "Number", "value": format!("#{number}")},
                {"title": "Status", "value": "\u{1f7e2} Open"},
                {"title": "URL", "value": url}
            ]}
        ],
        "actions": [
            {"type": "Action.Submit", "title": "\u{1f4dd} Create Another", "data": {"routeToCardId": "GH-issues-create", "action": "create_another"}},
            {"type": "Action.Submit", "title": "\u{2190} Back to Menu", "data": {"routeToCardId": "GH-connected", "step": "back"}}
        ]
    })
}

fn render_actions_card(data: &Value) -> Value {
    let runs = data["workflow_runs"].as_array().cloned().unwrap_or_default();
    let mut body: Vec<Value> = vec![json!({
        "type": "Container",
        "style": "emphasis",
        "items": [
            {"type": "TextBlock", "text": "\u{26a1} GitHub Actions", "size": "large", "weight": "bolder"},
            {"type": "TextBlock", "text": format!("{} recent runs", runs.len()), "size": "small", "isSubtle": true, "spacing": "none"}
        ]
    })];

    for run in &runs {
        let name = run["name"].as_str().unwrap_or("?");
        let display_title = run["display_title"].as_str().unwrap_or("");
        let branch = run["head_branch"].as_str().unwrap_or("?");
        let repo = run["repo"].as_str().unwrap_or("?");
        // Show just repo name without org prefix
        let repo_short = repo.rsplit('/').next().unwrap_or(repo);
        let status = run["status"].as_str().unwrap_or("unknown");
        let conclusion = run["conclusion"].as_str().unwrap_or("");
        let run_number = run["run_number"].as_u64().unwrap_or(0);
        let event = run["event"].as_str().unwrap_or("?");
        let (icon, status_text) = match (status, conclusion) {
            ("completed", "success") => ("\u{2705}", "Success"),
            ("completed", "failure") => ("\u{274c}", "Failed"),
            ("completed", "cancelled") => ("\u{23f9}", "Cancelled"),
            ("in_progress", _) => ("\u{1f7e1}", "Running"),
            ("queued", _) => ("\u{23f3}", "Queued"),
            _ => ("\u{2b55}", status),
        };

        let title_line = if !display_title.is_empty() && display_title != name {
            format!("{icon} {name} — {display_title}")
        } else {
            format!("{icon} {name}")
        };

        body.push(json!({
            "type": "Container",
            "style": "accent",
            "spacing": "small",
            "items": [
                {"type": "TextBlock", "text": title_line, "weight": "bolder", "wrap": true},
                {"type": "ColumnSet", "columns": [
                    {"type": "Column", "width": "auto", "items": [
                        {"type": "TextBlock", "text": format!("#{run_number}"), "size": "small", "isSubtle": true}
                    ]},
                    {"type": "Column", "width": "auto", "items": [
                        {"type": "TextBlock", "text": format!("{repo_short}/{branch}"), "size": "small", "isSubtle": true}
                    ]},
                    {"type": "Column", "width": "auto", "items": [
                        {"type": "TextBlock", "text": status_text, "size": "small", "weight": "bolder"}
                    ]},
                    {"type": "Column", "width": "auto", "items": [
                        {"type": "TextBlock", "text": event, "size": "small", "isSubtle": true}
                    ]}
                ]}
            ]
        }));
    }

    json!({
        "type": "AdaptiveCard",
        "version": "1.3",
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "body": body,
        "actions": [
            {"type": "Action.Submit", "title": "\u{2190} Back to Menu", "data": {"routeToCardId": "GH-connected", "step": "back"}}
        ]
    })
}

fn render_my_prs_card(data: &Value) -> Value {
    let prs = data["pull_requests"].as_array().cloned().unwrap_or_default();
    let user = data["user"].as_str().unwrap_or("you");
    let mut body: Vec<Value> = vec![json!({
        "type": "Container",
        "style": "emphasis",
        "items": [
            {"type": "TextBlock", "text": "\u{1f500} My Open Pull Requests", "size": "large", "weight": "bolder", "wrap": true},
            {"type": "TextBlock", "text": format!("{} open PRs by {user}", prs.len()), "size": "small", "isSubtle": true, "spacing": "none"}
        ]
    })];

    for pr in &prs {
        let number = pr["number"].as_u64().unwrap_or(0);
        let title = pr["title"].as_str().unwrap_or("?");
        let repo = pr["repo"].as_str().unwrap_or("?");
        let url = pr["html_url"].as_str().unwrap_or("");
        let draft = pr["draft"].as_bool().unwrap_or(false);
        let icon = if draft { "\u{1f4dd}" } else { "\u{1f7e2}" };

        let mut container = json!({
            "type": "Container",
            "style": "accent",
            "spacing": "small",
            "items": [
                {"type": "TextBlock", "text": format!("{icon} #{number} {title}"), "weight": "bolder", "wrap": true},
                {"type": "TextBlock", "text": format!("{repo} \u{2022} [Open on GitHub]({url})"), "size": "small", "isSubtle": true, "spacing": "none", "wrap": true}
            ]
        });
        if !url.is_empty() {
            container["selectAction"] = json!({"type": "Action.OpenUrl", "url": url});
        }
        body.push(container);
    }

    if prs.is_empty() {
        body.push(json!({
            "type": "Container", "style": "accent", "spacing": "small",
            "items": [{"type": "TextBlock", "text": "\u{2705} No open pull requests!", "wrap": true}]
        }));
    }

    json!({
        "type": "AdaptiveCard",
        "version": "1.3",
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "body": body,
        "actions": [
            {"type": "Action.Submit", "title": "\u{2190} Back to Menu", "data": {"routeToCardId": "GH-connected", "step": "back"}}
        ]
    })
}

fn render_notifications_card(data: &Value) -> Value {
    let notifs = data["notifications"].as_array().cloned().unwrap_or_default();
    let mut body: Vec<Value> = vec![json!({
        "type": "Container",
        "style": "emphasis",
        "items": [
            {"type": "TextBlock", "text": "\u{1f514} Notifications", "size": "large", "weight": "bolder"},
            {"type": "TextBlock", "text": format!("{} unread", notifs.len()), "size": "small", "isSubtle": true, "spacing": "none"}
        ]
    })];

    for n in &notifs {
        let title = n["title"].as_str().unwrap_or("?");
        let repo = n["repo"].as_str().unwrap_or("?");
        let reason = n["reason"].as_str().unwrap_or("?");
        let ntype = n["type"].as_str().unwrap_or("?");
        let icon = match ntype {
            "PullRequest" => "\u{1f500}",
            "Issue" => "\u{1f41b}",
            "Release" => "\u{1f4e6}",
            "Discussion" => "\u{1f4ac}",
            _ => "\u{1f514}",
        };
        let reason_label = match reason {
            "review_requested" => "Review requested",
            "mention" => "Mentioned",
            "author" => "Authored",
            "assign" => "Assigned",
            "comment" => "Comment",
            "ci_activity" => "CI activity",
            "subscribed" => "Subscribed",
            _ => reason,
        };

        body.push(json!({
            "type": "Container",
            "style": "accent",
            "spacing": "small",
            "items": [
                {"type": "TextBlock", "text": format!("{icon} {title}"), "weight": "bolder", "wrap": true},
                {"type": "TextBlock", "text": format!("{repo} \u{2022} {reason_label}"), "size": "small", "isSubtle": true, "spacing": "none"}
            ]
        }));
    }

    if notifs.is_empty() {
        body.push(json!({
            "type": "Container", "style": "good", "spacing": "small",
            "items": [{"type": "TextBlock", "text": "\u{2705} All caught up! No unread notifications.", "wrap": true}]
        }));
    }

    json!({
        "type": "AdaptiveCard",
        "version": "1.3",
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "body": body,
        "actions": [
            {"type": "Action.Submit", "title": "\u{2190} Back to Menu", "data": {"routeToCardId": "GH-connected", "step": "back"}}
        ]
    })
}

fn render_my_issues_card(data: &Value) -> Value {
    let issues = data["issues"].as_array().cloned().unwrap_or_default();
    let user = data["user"].as_str().unwrap_or("you");
    let filter = data["filter"].as_str().unwrap_or("involves");
    let filter_label = match filter {
        "authored" => "created by you",
        "assigned" => "assigned to you",
        _ => "involving you",
    };
    let mut body: Vec<Value> = vec![json!({
        "type": "Container",
        "style": "emphasis",
        "items": [
            {"type": "TextBlock", "text": "\u{1f41b} My Open Issues", "size": "large", "weight": "bolder", "wrap": true},
            {"type": "TextBlock", "text": format!("{} issues {filter_label}", issues.len()), "size": "small", "isSubtle": true, "spacing": "none"}
        ]
    })];

    for issue in &issues {
        let number = issue["number"].as_u64().unwrap_or(0);
        let title = issue["title"].as_str().unwrap_or("?");
        let repo = issue["repo"].as_str().unwrap_or("?");
        let url = issue["html_url"].as_str().unwrap_or("");
        let comments = issue["comments"].as_u64().unwrap_or(0);
        let author = issue["user"].as_str().unwrap_or("?");
        let labels: Vec<&str> = issue["labels"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|l| l.as_str()).collect())
            .unwrap_or_default();
        let label_text = if labels.is_empty() {
            String::new()
        } else {
            format!(" \u{2022} {}", labels.join(", "))
        };

        let author_info = if author == user {
            String::new()
        } else {
            format!(" by @{author}")
        };

        let mut container = json!({
            "type": "Container",
            "style": "accent",
            "spacing": "small",
            "items": [
                {"type": "TextBlock", "text": format!("\u{1f7e2} #{number} {title}"), "weight": "bolder", "wrap": true},
                {"type": "TextBlock", "text": format!("{repo}{author_info} \u{2022} \u{1f4ac} {comments}{label_text}"), "size": "small", "isSubtle": true, "spacing": "none", "wrap": true}
            ]
        });
        if !url.is_empty() {
            container["selectAction"] = json!({"type": "Action.OpenUrl", "url": url});
        }
        body.push(container);
    }

    if issues.is_empty() {
        body.push(json!({
            "type": "Container", "style": "good", "spacing": "small",
            "items": [{"type": "TextBlock", "text": "\u{2705} No open issues!", "wrap": true}]
        }));
    }

    json!({
        "type": "AdaptiveCard",
        "version": "1.3",
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "body": body,
        "actions": [
            {"type": "Action.Submit", "title": "\u{1f465} Involving Me", "data": {"action": "mcp", "tool": "search_my_issues", "owner": "", "args": "{\"per_page\":8,\"filter\":\"involves\"}"}},
            {"type": "Action.Submit", "title": "\u{270d} Created by Me", "data": {"action": "mcp", "tool": "search_my_issues", "owner": "", "args": "{\"per_page\":8,\"filter\":\"authored\"}"}},
            {"type": "Action.Submit", "title": "\u{1f4cc} Assigned to Me", "data": {"action": "mcp", "tool": "search_my_issues", "owner": "", "args": "{\"per_page\":8,\"filter\":\"assigned\"}"}},
            {"type": "Action.Submit", "title": "\u{2190} Back to Menu", "data": {"routeToCardId": "GH-connected", "step": "back"}}
        ]
    })
}
