use std::fs;

use tempfile::tempdir;

use super::{analyze_project, CanvasEdgeKind, CanvasNodeKind, ProjectKind};

#[test]
fn discovers_next_pages_api_calls_and_request_body() {
    let root = tempdir().expect("create temp project");
    fs::write(
        root.path().join("package.json"),
        r#"{"dependencies":{"next":"latest","react":"latest"}}"#,
    )
    .expect("write package manifest");
    fs::create_dir_all(root.path().join("app/about")).expect("create about page");
    fs::create_dir_all(root.path().join("app/api/users")).expect("create API route");
    fs::write(
        root.path().join("app/page.tsx"),
        r#"import Link from "next/link";
export default function Home() {
  fetch("/api/users", { method: "POST" });
  return <Link href="/about">About</Link>;
}"#,
    )
    .expect("write home page");
    fs::write(
        root.path().join("app/about/page.tsx"),
        "export default function About() { return <main>About</main>; }",
    )
    .expect("write about page");
    fs::write(
        root.path().join("app/api/users/route.ts"),
        r#"export async function POST(request: Request) {
  const { name, email } = await request.json();
  return Response.json({ name, email });
}"#,
    )
    .expect("write API route");

    let graph = analyze_project(root.path());

    assert_eq!(graph.kind, ProjectKind::FullStack);
    assert_eq!(graph.page_count(), 2);
    assert_eq!(graph.endpoint_count(), 1);
    assert_eq!(
        graph
            .nodes
            .iter()
            .find(|node| node.kind == CanvasNodeKind::Endpoint)
            .and_then(|node| node.request_body.as_deref()),
        Some("JSON fields: name, email")
    );
    assert_eq!(
        graph
            .edges
            .iter()
            .any(|edge| edge.kind == CanvasEdgeKind::PageLink),
        true
    );
    assert_eq!(
        graph
            .edges
            .iter()
            .any(|edge| edge.kind == CanvasEdgeKind::ApiCall && edge.line == 3),
        true
    );
}

#[test]
fn discovers_express_endpoint_and_body_fields() {
    let root = tempdir().expect("create temp project");
    fs::write(
        root.path().join("server.js"),
        r#"const express = require("express");
const app = express();
app.post("/users", (req, res) => {
  const { name, email } = req.body;
  res.json({ name, email });
});"#,
    )
    .expect("write Express server");

    let graph = analyze_project(root.path());
    let endpoint = graph
        .nodes
        .iter()
        .find(|node| node.kind == CanvasNodeKind::Endpoint)
        .expect("Express endpoint");

    assert_eq!(graph.kind, ProjectKind::Backend);
    assert_eq!(endpoint.method.as_deref(), Some("POST"));
    assert_eq!(endpoint.route.as_deref(), Some("/users"));
    assert_eq!(
        endpoint.request_body.as_deref(),
        Some("JSON fields: name, email")
    );
    assert_eq!(
        graph
            .nodes
            .iter()
            .any(|node| node.kind == CanvasNodeKind::Router),
        true
    );
}

#[test]
fn reports_unknown_for_project_without_routes() {
    let root = tempdir().expect("create temp project");
    fs::write(root.path().join("main.rs"), "fn main() {}").expect("write route-free source");

    let graph = analyze_project(root.path());

    assert_eq!(graph.kind, ProjectKind::Unknown);
    assert_eq!(graph.nodes.len(), 0);
    assert_eq!(graph.edges.len(), 0);
}
