use axum::{response::Html, Json};
use serde::Serialize;

#[derive(Serialize)]
pub struct DashboardInfo {
    name: &'static str,
    version: &'static str,
    ui: &'static str,
}

pub async fn dashboard() -> Html<&'static str> {
    Html(include_str!("../web/dashboard.html"))
}

pub async fn dashboard_info() -> Json<DashboardInfo> {
    Json(DashboardInfo {
        name: "TuxBridge Mission Control",
        version: env!("CARGO_PKG_VERSION"),
        ui: "/ui",
    })
}
