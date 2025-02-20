mod minitrace_actix;

use std::sync::Arc;

use actix_web::{
  dev::Response,
  middleware::Compat,
  route,
  web::{self, Bytes},
  App, HttpRequest, HttpResponse, HttpServer, Responder, Scope,
};
use conductor_common::http::{ConductorHttpRequest, ConductorHttpResponse, HttpHeadersMap};
use conductor_config::load_config;
use conductor_engine::gateway::{ConductorGateway, ConductorGatewayRouteData};
use conductor_tracing::minitrace_mgr::MinitraceManager;
use minitrace::{collector::Config, trace};
use tracing::{debug, error};
use tracing_subscriber::{layer::SubscriberExt, registry};

use crate::minitrace_actix::MinitraceTransform;

pub async fn run_services(config_file_path: &String) -> std::io::Result<()> {
  let config = load_config(config_file_path, |key| std::env::var(key).ok()).await;
  let logger_config = config.logger.clone().unwrap_or_default();
  let logger = conductor_logger::logger_layer::build_logger(
    &logger_config.format,
    &logger_config.filter,
    logger_config.print_performance_info,
  )
  .unwrap_or_else(|e| panic!("failed to build logger: {}", e));
  let mut tracing_manager = MinitraceManager::default();

  match ConductorGateway::new(&config, &mut tracing_manager).await {
    Ok(gw) => {
      let subscriber = registry::Registry::default().with(logger);
      // @expected: we need to exit the process, if the logger can't be correctly set.
      tracing::subscriber::set_global_default(subscriber).expect("failed to set up tracing");
      let tracing_reporter = tracing_manager.build_root_reporter();
      minitrace::set_reporter(tracing_reporter, Config::default());

      let gateway = Arc::new(gw);
      let http_server = HttpServer::new(move || {
        let mut router = App::new();

        for conductor_route in gateway.routes.iter() {
          let child_router = Scope::new(conductor_route.base_path.as_str())
            .wrap(Compat::new(MinitraceTransform::new()))
            .app_data(web::Data::new(conductor_route.route_data.clone()))
            .service(Scope::new("").default_service(
              web::route().to(handler), // handle all requests with this handler
            ));

          router = router.service(child_router)
        }

        router.service(health_handler)
      });

      let server_config = config.server.clone().unwrap_or_default();
      let server_address = format!("{}:{}", server_config.host, server_config.port);
      debug!("server is trying to listen on {:?}", server_address);

      let server_instance = http_server
        .bind((server_config.host, server_config.port))?
        .run()
        .await;

      tracing_manager.shutdown().await;

      server_instance
    }
    Err(e) => {
      error!("failed to initialize gateway: {:?}", e);
      // @expected: we need to exit the process, if the provided configuration file is incorrect.
      panic!("Failed to initialize gateway: {:?}", e);
    }
  }
}

#[route("/_health", method = "GET", method = "HEAD")]
async fn health_handler() -> impl Responder {
  Response::ok()
}

#[trace(name = "transform_request")]
fn transform_req(req: HttpRequest, body: Bytes) -> ConductorHttpRequest {
  let mut headers_map = HttpHeadersMap::new();

  for (key, value) in req.headers().into_iter() {
    headers_map.insert(key, value.clone());
  }

  let conductor_request = ConductorHttpRequest {
    body,
    headers: headers_map,
    method: req.method().clone(),
    uri: req.uri().to_string(),
    query_string: req.query_string().to_string(),
  };

  conductor_request
}

#[trace(name = "transform_response")]
fn transform_res(conductor_response: ConductorHttpResponse) -> HttpResponse {
  let mut response = HttpResponse::build(conductor_response.status);

  for (key, value) in conductor_response.headers.iter() {
    response.insert_header((key, value));
  }

  response.body(conductor_response.body)
}

async fn handler(
  req: HttpRequest,
  body: Bytes,
  route_data: web::Data<Arc<ConductorGatewayRouteData>>,
) -> impl Responder {
  let conductor_request = transform_req(req, body);
  let conductor_response: ConductorHttpResponse =
    ConductorGateway::execute(conductor_request, &route_data).await;

  transform_res(conductor_response)
}
