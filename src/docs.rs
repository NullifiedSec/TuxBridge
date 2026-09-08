use axum::{
    http::{HeaderValue, header::CONTENT_TYPE},
    response::{Html, IntoResponse, Response},
};

const SCALAR_HTML: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>TuxBridge Agent API</title>
  </head>
  <body>
    <div id="app"></div>
    <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
    <script>
      Scalar.createApiReference('#app', {
        url: '/openapi-agent-v2.yaml',
        showOperationId: true,
      })
    </script>
  </body>
</html>
"#;

pub async fn scalar_reference() -> Html<&'static str> {
    Html(SCALAR_HTML)
}

pub async fn agent_openapi() -> Response {
    let mut response = include_str!("../openapi-agent-v2.yaml").into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/yaml; charset=utf-8"),
    );
    response
}
