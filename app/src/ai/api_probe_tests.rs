use std::fs;

use tempfile::tempdir;

use super::*;

#[test]
fn headers_parse_as_pairs_and_skip_comments() {
    let headers = parse_headers(
        "Authorization: Bearer abc123\n# a comment\n\nAccept: application/json\nX-Empty:",
    );
    assert_eq!(
        headers,
        vec![
            ("Authorization".to_string(), "Bearer abc123".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
            ("X-Empty".to_string(), String::new()),
        ]
    );
}

#[test]
fn headers_keep_duplicates_and_values_containing_colons() {
    let headers = parse_headers("Accept: a\nAccept: b\nReferer: http://localhost:3000/x");
    assert_eq!(headers.len(), 3);
    assert_eq!(headers[2].1, "http://localhost:3000/x");
}

#[test]
fn urls_join_without_doubling_or_dropping_slashes() {
    assert_eq!(
        build_url("http://127.0.0.1:3000/", "/api/users"),
        "http://127.0.0.1:3000/api/users"
    );
    assert_eq!(
        build_url("http://127.0.0.1:3000", "api/users"),
        "http://127.0.0.1:3000/api/users"
    );
    assert_eq!(build_url("http://127.0.0.1:3000", ""), "http://127.0.0.1:3000");
}

#[test]
fn an_absolute_route_overrides_the_base() {
    assert_eq!(
        build_url("http://127.0.0.1:3000", "https://api.example.com/v1"),
        "https://api.example.com/v1"
    );
}

#[test]
fn only_body_carrying_methods_send_one() {
    assert!(method_takes_body("POST"));
    assert!(method_takes_body("patch"));
    assert!(!method_takes_body("GET"));
    assert!(!method_takes_body("head"));
    assert!(!method_takes_body("OPTIONS"));
}

#[test]
fn path_placeholders_are_listed_across_conventions() {
    let route = ProbeRoute {
        id: "endpoint:GET:/users/:id/posts/[postId]".to_string(),
        method: "GET".to_string(),
        route: "/users/:id/posts/[postId]/{slug}".to_string(),
        source: PathBuf::from("routes.ts"),
        line: 1,
        body_hint: None,
    };
    assert_eq!(route.placeholders(), vec!["id", "postId", "slug"]);
}

#[test]
fn a_route_without_parameters_has_no_placeholders() {
    let route = ProbeRoute {
        id: "endpoint:GET:/health".to_string(),
        method: "GET".to_string(),
        route: "/health".to_string(),
        source: PathBuf::from("routes.ts"),
        line: 1,
        body_hint: None,
    };
    assert!(route.placeholders().is_empty());
}

#[test]
fn declared_ports_are_read_from_config_and_source() {
    let dir = tempdir().expect("temp dir");
    fs::write(dir.path().join(".env"), "PORT=4321\nDATABASE_URL=x\n").expect("write .env");
    fs::write(
        dir.path().join("server.js"),
        "const app = express();\napp.listen(8123);\n",
    )
    .expect("write server.js");

    let ports = declared_ports(dir.path())
        .into_iter()
        .map(|port| port.port)
        .collect::<Vec<_>>();
    assert!(ports.contains(&4321), "expected 4321 in {ports:?}");
    assert!(ports.contains(&8123), "expected 8123 in {ports:?}");
}

#[test]
fn values_too_small_to_be_a_dev_port_are_ignored() {
    let dir = tempdir().expect("temp dir");
    // `port: 5` here is a retry count, not a port — the kind of false positive
    // that would otherwise fill the sidebar with dead entries.
    fs::write(dir.path().join("config.ts"), "export const retry = { port: 5 };\n")
        .expect("write config.ts");

    assert!(declared_ports(dir.path()).is_empty());
}

#[test]
fn declared_ports_skip_dependency_directories() {
    let dir = tempdir().expect("temp dir");
    let nested = dir.path().join("node_modules").join("pkg");
    fs::create_dir_all(&nested).expect("create node_modules");
    fs::write(nested.join("index.js"), "app.listen(9999);\n").expect("write dep");

    assert!(declared_ports(dir.path()).is_empty());
}

#[test]
fn json_bodies_are_reindented_and_other_types_are_left_alone() {
    let pretty = prettify(r#"{"a":1,"b":[2,3]}"#, Some("application/json"));
    assert!(pretty.contains("\n  \"a\": 1"), "got {pretty}");

    let html = "<html><body>hi</body></html>";
    assert_eq!(prettify(html, Some("text/html")), html);
}

#[test]
fn malformed_json_is_shown_exactly_as_received() {
    let broken = r#"{"a": 1,,}"#;
    assert_eq!(prettify(broken, Some("application/json")), broken);
}

#[test]
fn an_empty_url_is_rejected_before_any_request_is_made() {
    let error = futures::executor::block_on(send_request(ProbeRequest {
        method: "GET".to_string(),
        url: "   ".to_string(),
        headers: String::new(),
        body: String::new(),
    }))
    .expect_err("an empty URL must not be sent");
    assert!(error.to_string().contains("Enter a URL"));
}

#[test]
fn loopback_urls_are_recognised_in_every_form_people_type() {
    // These must all bypass the system proxy: a request to this machine that
    // gets handed to a proxy is the difference between 20 ms and 15 seconds.
    for url in [
        "http://localhost:3000/api",
        "http://127.0.0.1:8080",
        "127.0.0.1:3210/health",
        "localhost:5173",
        "http://[::1]:9000/x",
        "http://0.0.0.0:4000",
        "http://127.5.5.5:1234",
        "http://user:pw@localhost:3000/api",
    ] {
        assert!(is_loopback_url(url), "{url} should be loopback");
    }
}

#[test]
fn external_urls_still_go_through_the_system_proxy() {
    for url in [
        "https://api.example.com/v1",
        "http://192.168.1.10:8080",
        // A host that merely mentions localhost is not localhost.
        "https://localhost.example.com/api",
        "https://notlocalhost/api",
    ] {
        assert!(!is_loopback_url(url), "{url} should not be loopback");
    }
}

#[test]
fn the_loopback_and_external_clients_are_each_built_once() {
    // The bug this guards: a client rebuilt per request reloads the system
    // proxy configuration every time, which this codebase already documents as
    // slow (see `http_client::Client::new_for_test`).
    let first = client_for("http://127.0.0.1:3000/a") as *const reqwest::Client;
    let second = client_for("http://localhost:9999/b") as *const reqwest::Client;
    assert!(std::ptr::eq(first, second), "loopback client must be reused");

    let external = client_for("https://example.com") as *const reqwest::Client;
    assert!(
        !std::ptr::eq(first, external),
        "external traffic must not borrow the no-proxy client"
    );
}

#[test]
fn response_sizes_render_in_human_units() {
    let response = |size| ProbeResponse {
        status: 200,
        status_text: "OK".to_string(),
        headers: Vec::new(),
        body: String::new(),
        elapsed_ms: 1,
        size_bytes: size,
        content_type: None,
        truncated: false,
    };
    assert_eq!(response(340).size_label(), "340 B");
    assert_eq!(response(2048).size_label(), "2.0 KB");
    assert_eq!(response(3 * 1024 * 1024).size_label(), "3.0 MB");
}
