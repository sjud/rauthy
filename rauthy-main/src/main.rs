// Rauthy - OpenID Connect and Single Sign-On Identity & Access Management
// Copyright (C) 2023 Sebastian Dobe <sebastiandobe@mailbox.org>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use actix_web::rt::System;
use actix_web::{middleware, web, App, HttpServer};
use actix_web_prom::PrometheusMetricsBuilder;
use prometheus::Registry;
use rauthy_common::constants::{
    CACHE_NAME_12HR, CACHE_NAME_AUTH_CODES, CACHE_NAME_LOGIN_DELAY, CACHE_NAME_POW,
    CACHE_NAME_SESSIONS, CACHE_NAME_WEBAUTHN, CACHE_NAME_WEBAUTHN_DATA, POW_EXP, RAUTHY_VERSION,
    SWAGGER_UI_EXTERNAL, SWAGGER_UI_INTERNAL, WEBAUTHN_DATA_EXP, WEBAUTHN_REQ_EXP,
};
use rauthy_common::error_response::ErrorResponse;
use rauthy_common::password_hasher;
use rauthy_handlers::middleware::ip_blacklist::RauthyIpBlacklistMiddleware;
use rauthy_handlers::middleware::logging::RauthyLoggingMiddleware;
use rauthy_handlers::middleware::principal::RauthyPrincipalMiddleware;
use rauthy_handlers::openapi::ApiDoc;
use rauthy_handlers::{clients, events, generic, groups, oidc, roles, scopes, sessions, users};
use rauthy_models::app_state::{AppState, Caches, DbPool};
use rauthy_models::email::EMail;
use rauthy_models::events::event::Event;
use rauthy_models::events::health_watch::watch_health;
use rauthy_models::events::listener::EventListener;
use rauthy_models::events::{init_event_vars, ip_blacklist_handler};
use rauthy_models::{email, ListenScheme};
use sqlx::{query, query_as};
use std::error::Error;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::time::Duration;
use std::{env, thread};
use tokio::sync::mpsc;
use tokio::time;
use tracing::{debug, error, info};
use utoipa_swagger_ui::SwaggerUi;

use crate::cache_notify::handle_notify;
use crate::logging::setup_logging;

mod cache_notify;
mod logging;
mod schedulers;
mod tls;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!(
        r#"
                                          88
                                    ,d    88
                                    88    88
8b,dPPYba, ,adPPYYba, 88       88 MM88MMM 88,dPPYba,  8b       d8
88P'   "Y8 ""     `Y8 88       88   88    88P'    "8a `8b     d8'
88         ,adPPPPP88 88       88   88    88       88  `8b   d8'
88         88,    ,88 "8a,   ,a88   88,   88       88   `8b,d8'
88         `"8bbdP"Y8  `"YbbdP'Y8   "Y888 88       88     Y88'
                                                          d8'
                                                         d8'
    "#
    );
    // This sleep is just a test. On some terminals, the banner gets mixed up with the first other
    // logs. We dont care about rauthy's startup time being 1ms longer.
    time::sleep(Duration::from_millis(1)).await;

    // setup logging
    let mut test_mode = false;
    let args: Vec<String> = env::args().collect();
    if args.len() > 1 && args[1] == "test" {
        test_mode = true;
        dotenvy::from_filename("rauthy.test.cfg").ok();
    } else {
        dotenvy::from_filename("rauthy.cfg").expect("'rauthy.cfg' error");
    }

    let log_level = setup_logging();

    info!("Starting Rauthy v{}", RAUTHY_VERSION);
    info!("Log Level set to '{}'", log_level);
    if test_mode {
        info!("Application started in Integration Test Mode");
    }

    // caches
    let (tx_health_state, mut cache_config) = redhac::CacheConfig::new();

    // "infinity" cache
    cache_config.spawn_cache(
        CACHE_NAME_12HR.to_string(),
        redhac::TimedCache::with_lifespan(43200),
        Some(32),
    );

    // auth codes
    cache_config.spawn_cache(
        CACHE_NAME_AUTH_CODES.to_string(),
        redhac::TimedCache::with_lifespan(300 + *WEBAUTHN_REQ_EXP),
        Some(64),
    );

    // sessions
    let session_lifetime = env::var("SESSION_LIFETIME")
        .unwrap_or_else(|_| String::from("14400"))
        .trim()
        .parse::<u32>()
        .expect("SESSION_LIFETIME cannot be parsed to u32 - bad format");
    cache_config.spawn_cache(
        CACHE_NAME_SESSIONS.to_string(),
        redhac::TimedCache::with_lifespan(session_lifetime as u64),
        Some(64),
    );

    // PoWs
    cache_config.spawn_cache(
        CACHE_NAME_POW.to_string(),
        redhac::TimedCache::with_lifespan(*POW_EXP),
        Some(16),
    );

    // webauthn requests
    cache_config.spawn_cache(
        CACHE_NAME_WEBAUTHN.to_string(),
        redhac::TimedCache::with_lifespan(*WEBAUTHN_REQ_EXP),
        Some(32),
    );
    cache_config.spawn_cache(
        CACHE_NAME_WEBAUTHN_DATA.to_string(),
        redhac::TimedCache::with_lifespan(*WEBAUTHN_DATA_EXP),
        Some(32),
    );

    // login delay cache
    cache_config.spawn_cache(
        CACHE_NAME_LOGIN_DELAY.to_string(),
        redhac::SizedCache::with_size(1),
        Some(16),
    );

    // The ha cache must be started after all entries have been added to the cache map
    let (tx_notify, rx_notify) = mpsc::channel(64);
    redhac::start_cluster(tx_health_state, &mut cache_config, Some(tx_notify), None).await?;

    // email sending
    let (tx_email, rx_email) = mpsc::channel::<EMail>(16);
    tokio::spawn(email::sender(rx_email, test_mode));

    // build the application state
    let caches = Caches {
        ha_cache_config: cache_config.clone(),
    };

    let (tx_events, rx_events) = flume::unbounded();
    let (tx_events_router, rx_events_router) = flume::unbounded();
    let (tx_ip_blacklist, rx_ip_blacklist) = flume::unbounded();

    let app_state = web::Data::new(
        AppState::new(
            tx_email.clone(),
            tx_events.clone(),
            tx_events_router.clone(),
            tx_ip_blacklist.clone(),
            caches,
        )
        .await?,
    );

    // TODO remove with v0.17
    TEMP_migrate_passkeys_uv(&app_state.db)
        .await
        .expect("Passkey UV migration to not fail");

    // events listener
    init_event_vars().unwrap();
    tokio::spawn(EventListener::listen(
        tx_email,
        tx_ip_blacklist.clone(),
        tx_events_router,
        rx_events_router,
        rx_events,
        app_state.db.clone(),
    ));

    // // TODO REMOVE AFTER TESTING
    // let txe = tx_events.clone();
    // // let data = app_state.clone();
    // tokio::spawn(async move {
    //     // use rauthy_models::entity::api_keys::{
    //     //     AccessGroup, AccessRights, ApiKeyAccess, ApiKeyEntity,
    //     // };
    //     // use rauthy_models::events::event::Event;
    //     //
    //     // time::sleep(Duration::from_secs(2)).await;
    //     // let key = ApiKeyEntity::create(
    //     //     &data,
    //     //     "test".to_string(),
    //     //     None,
    //     //     vec![ApiKeyAccess {
    //     //         group: AccessGroup::Events,
    //     //         access_rights: vec![AccessRights::Read, AccessRights::Create],
    //     //     }],
    //     // )
    //     // .await
    //     // .unwrap();
    //     // println!("\n\ntest${}\n", key);
    //
    //     use chrono::Utc;
    //     use std::ops::Add;
    //
    //     loop {
    //         let ip = "255.255.255.255".to_string();
    //         let email = "sebastiandobe@mailbox.org".to_string();
    //
    //         time::sleep(Duration::from_secs(1)).await;
    //         rauthy_models::events::event::Event::invalid_login(3, ip.clone())
    //             .send(&txe)
    //             .await
    //             .unwrap();
    //
    //         time::sleep(Duration::from_secs(1)).await;
    //         rauthy_models::events::event::Event::invalid_login(8, ip.clone())
    //             .send(&txe)
    //             .await
    //             .unwrap();
    //
    //         time::sleep(Duration::from_secs(1)).await;
    //         rauthy_models::events::event::Event::invalid_login(13, ip.clone())
    //             .send(&txe)
    //             .await
    //             .unwrap();
    //
    //         time::sleep(Duration::from_secs(1)).await;
    //         rauthy_models::events::event::Event::ip_blacklisted(
    //             Utc::now().add(chrono::Duration::hours(2)),
    //             ip.clone(),
    //         )
    //         .send(&txe)
    //         .await
    //         .unwrap();
    //
    //         time::sleep(Duration::from_secs(1)).await;
    //         rauthy_models::events::event::Event::new_user(email.clone(), Some(ip.clone()))
    //             .send(&txe)
    //             .await
    //             .unwrap();
    //
    //         time::sleep(Duration::from_secs(1)).await;
    //         rauthy_models::events::event::Event::new_rauthy_admin(email.clone(), Some(ip.clone()))
    //             .send(&txe)
    //             .await
    //             .unwrap();
    //
    //         time::sleep(Duration::from_secs(1)).await;
    //         rauthy_models::events::event::Event::jwks_rotated()
    //             .send(&txe)
    //             .await
    //             .unwrap();
    //
    //         time::sleep(Duration::from_secs(1)).await;
    //         rauthy_models::events::event::Event::rauthy_unhealthy_cache()
    //             .send(&txe)
    //             .await
    //             .unwrap();
    //
    //         time::sleep(Duration::from_secs(1)).await;
    //         rauthy_models::events::event::Event::rauthy_unhealthy_db()
    //             .send(&txe)
    //             .await
    //             .unwrap();
    //
    //         time::sleep(Duration::from_secs(1)).await;
    //         rauthy_models::events::event::Event::secrets_migrated(Some(ip.clone()))
    //             .send(&txe)
    //             .await
    //             .unwrap();
    //
    //         time::sleep(Duration::from_secs(1)).await;
    //         rauthy_models::events::event::Event::user_email_change(
    //             "sebastian.dobe@gmail.com -> sebastiandobe@mailbox.org".to_string(),
    //             Some(ip),
    //         )
    //         .send(&txe)
    //         .await
    //         .unwrap();
    //
    //         time::sleep(Duration::from_secs(10)).await;
    //     }
    // });

    // spawn password hash limiter
    tokio::spawn(password_hasher::run());

    // spawn ip blacklist handler
    tokio::spawn(ip_blacklist_handler::run(tx_ip_blacklist, rx_ip_blacklist));

    // spawn remote cache notification service
    tokio::spawn(handle_notify(app_state.clone(), rx_notify));

    // spawn health watcher
    tokio::spawn(watch_health(
        app_state.db.clone(),
        app_state.tx_events.clone(),
        app_state.caches.ha_cache_config.rx_health_state.clone(),
    ));

    // schedulers
    match env::var("SCHED_DISABLE")
        .unwrap_or_else(|_| String::from("false"))
        .as_str()
    {
        "true" => {
            info!("Schedulers are disabled");
        }
        _ => {
            tokio::spawn(schedulers::scheduler_main(app_state.clone()));
        }
    };

    // make sure, that all caches are cleared from possible inconsistent leftovers from the migrations
    if let Err(err) = redhac::clear_caches(&cache_config).await {
        error!("Error clearing cache after migrations: {}", err.error);
    }

    // actix web
    let state = app_state.clone();
    let actix = thread::spawn(move || {
        let actix_system = actix_web::rt::System::new();
        actix_system.block_on(actix_main(state)).map_err(|e| {
            error!("{}", e);
        })
    });

    actix.join().unwrap().unwrap();
    app_state.caches.ha_cache_config.shutdown().await.unwrap();

    Ok(())
}

// #[actix_web::main]
async fn actix_main(app_state: web::Data<AppState>) -> std::io::Result<()> {
    debug!(
        "Actix Main Thread is running on {:?}",
        thread::current().id()
    );

    let listen_scheme = app_state.listen_scheme.clone();
    let listen_addr = app_state.listen_addr.clone();

    // custom number of workers
    let mut workers = env::var("HTTP_WORKERS")
        .unwrap_or_else(|_| String::from("0"))
        .parse::<usize>()
        .expect("Unable to parse HTTP_WORKERS");
    if workers == 0 {
        workers = num_cpus::get();
    }

    // OpenAPI / Swagger
    let swagger = SwaggerUi::new("/docs/v1/swagger-ui/{_:.*}")
        .url("/docs/v1/api-doc/openapi.json", ApiDoc::build(&app_state))
        .config(
            utoipa_swagger_ui::Config::from("../api-doc/openapi.json").try_it_out_enabled(false),
        );

    // Prometheus metrics
    let metrics_enable = env::var("METRICS_ENABLE")
        .unwrap_or_else(|_| "true".to_string())
        .parse::<bool>()
        .expect("Cannot parse METRICS_ENABLE to bool");
    let pub_metrics = if metrics_enable {
        let shared_registry = Registry::new();
        let metrics = PrometheusMetricsBuilder::new("api")
            .registry(shared_registry.clone())
            .endpoint("/metrics")
            .exclude("/favicon.ico")
            .exclude("/metrics")
            .build()
            .unwrap();

        let swagger_clone = swagger.clone();
        thread::spawn(move || {
            let addr = env::var("METRICS_ADDR").unwrap_or_else(|_| "0.0.0.0".to_string());
            let port = env::var("METRICS_PORT").unwrap_or_else(|_| "9090".to_string());
            if let Err(err) = Ipv4Addr::from_str(&addr) {
                let msg = format!("Error parsing METRICS_ADDR: {}", err);
                error!(msg);
                panic!("{}", msg);
            }
            let addr_full = format!("{}:{}", addr, port);

            info!("Metrics available on: http://{}/metrics", addr_full);
            let srv = if *SWAGGER_UI_INTERNAL {
                info!(
                    "Serving Swagger UI internally on: http://{}/docs/v1/swagger-ui/",
                    addr_full
                );
                HttpServer::new(move || {
                    App::new()
                        .wrap(metrics.clone())
                        .service(swagger_clone.clone())
                })
                .workers(1)
                .bind(addr_full)
                .unwrap()
                .run()
            } else {
                HttpServer::new(move || App::new().wrap(metrics.clone()))
                    .workers(1)
                    .bind(addr_full)
                    .unwrap()
                    .run()
            };
            System::new().block_on(srv).unwrap();
        });

        PrometheusMetricsBuilder::new("rauthy")
            .registry(shared_registry)
            // no endpoint means it will not expose one, only collect data
            .exclude("/favicon.ico")
            .exclude("/metrics")
            .build()
            .unwrap()
    } else {
        PrometheusMetricsBuilder::new("rauthy")
            // no endpoint means it will not expose one, only collect data
            .exclude_regex(".*")
            .build()
            .unwrap()
    };

    // send start event
    app_state
        .tx_events
        .send_async(Event::rauthy_started())
        .await
        .unwrap();

    // Note: all .wrap's are executed in reverse order -> the last .wrap is executed as the first
    // one for any new request
    let server = HttpServer::new(move || {
        let mut app = App::new()
            // .data shares application state for all workers
            .app_data(app_state.clone())
            .wrap(RauthyPrincipalMiddleware)
            .wrap(RauthyLoggingMiddleware)
            .wrap(
                middleware::DefaultHeaders::new()
                    .add(("x-frame-options", "SAMEORIGIN"))
                    .add(("x-xss-protection", "1;mode=block"))
                    .add(("x-content-type-options", "nosniff"))
                    .add((
                        "strict-transport-security",
                        "max-age=31536000;includeSubDomains",
                    ))
                    .add(("referrer-policy", "no-referrer"))
                    .add(("x-robots-tag", "none"))
                    .add((
                        "content-security-policy",
                        "default-src 'self'; script-src 'self'; style-src 'self'; frame-ancestors 'self'; object-src 'none'; img-src 'self' data:;",
                    ))
                    .add(("cache-control", "no-store"))
                    .add(("pragma", "no-cache")),
            )
            .wrap(pub_metrics.clone())
            .service(generic::redirect)
            // Important: Do not move this middleware do need the least amount of computing
            // for blacklisted IPs -> middlewares are executed in reverse order -> this one first
            .wrap(RauthyIpBlacklistMiddleware)
            .service(
                    web::scope("/auth")
                    .service(generic::redirect_v1)
                    .service(
                    web::scope("/v1")
                        .service(events::sse_events)
                        .service(events::post_event_test)
                        .service(generic::get_index)
                        .service(generic::get_account_html)
                        .service(generic::get_admin_html)
                        .service(generic::get_admin_attr_html)
                        .service(generic::get_admin_clients_html)
                        .service(generic::get_admin_config_html)
                        .service(generic::get_admin_docs_html)
                        .service(generic::get_admin_groups_html)
                        .service(generic::get_admin_roles_html)
                        .service(generic::get_admin_scopes_html)
                        .service(generic::get_admin_sessions_html)
                        .service(generic::get_admin_users_html)
                        .service(generic::get_auth_check)
                        .service(generic::get_auth_check_admin)
                        .service(generic::post_i18n)
                        .service(generic::post_update_language)
                        .service(oidc::get_authorize)
                        .service(oidc::post_authorize)
                        .service(oidc::get_callback_html)
                        // .service(oidc::post_authorize_refresh)
                        .service(oidc::get_certs)
                        .service(oidc::get_cert_by_kid)
                        .service(oidc::get_logout)
                        .service(oidc::post_logout)
                        .service(oidc::rotate_jwk)
                        .service(oidc::get_session_info)
                        .service(oidc::get_session_xsrf)
                        .service(clients::get_clients)
                        .service(clients::get_client_by_id)
                        .service(clients::get_client_colors)
                        .service(clients::put_client_colors)
                        .service(clients::delete_client_colors)
                        .service(clients::get_client_logo)
                        .service(clients::put_client_logo)
                        .service(clients::delete_client_logo)
                        .service(clients::get_client_secret)
                        .service(clients::post_clients)
                        .service(clients::put_clients)
                        .service(clients::put_generate_client_secret)
                        .service(clients::delete_client)
                        .service(generic::get_login_time)
                        .service(users::get_users)
                        .service(users::get_users_register)
                        .service(users::post_users_register)
                        .service(users::get_cust_attr)
                        .service(users::post_cust_attr)
                        .service(users::put_cust_attr)
                        .service(users::delete_cust_attr)
                        .service(users::get_user_by_id)
                        .service(users::get_user_attr)
                        .service(users::put_user_attr)
                        .service(users::get_user_email_confirm)
                        .service(users::post_user_self_convert_passkey)
                        .service(generic::post_password_hash_times)
                        .service(sessions::get_sessions)
                        .service(sessions::delete_sessions)
                        .service(sessions::delete_sessions_for_user)
                        .service(users::get_user_password_reset)
                        .service(users::put_user_password_reset)
                        .service(users::get_user_by_email)
                        .service(users::post_users)
                        .service(users::put_user_by_id)
                        .service(users::put_user_self)
                        .service(users::delete_user_by_id)
                        .service(users::post_user_password_request_reset)
                        .service(users::get_user_webauthn_passkeys)
                        .service(users::post_webauthn_reg_start)
                        .service(users::post_webauthn_reg_finish)
                        .service(users::post_webauthn_auth_start)
                        .service(users::post_webauthn_auth_finish)
                        .service(users::delete_webauthn)
                        .service(generic::get_password_policy)
                        .service(generic::put_password_policy)
                        .service(generic::get_pow)
                        .service(oidc::post_refresh_token)
                        .service(groups::get_groups)
                        .service(groups::post_group)
                        .service(groups::put_group)
                        .service(groups::delete_group)
                        .service(roles::get_roles)
                        .service(roles::post_role)
                        .service(roles::put_role)
                        .service(roles::delete_role)
                        .service(scopes::get_scopes)
                        .service(scopes::post_scope)
                        .service(scopes::put_scope)
                        .service(scopes::delete_scope)
                        .service(oidc::post_token)
                        .service(oidc::post_token_info)
                        .service(oidc::get_userinfo)
                        .service(generic::get_enc_keys)
                        .service(generic::post_migrate_enc_key)
                        .service(generic::ping)
                        .service(oidc::post_validate_token)
                        .service(oidc::get_well_known)
                        .service(generic::get_health)
                        .service(generic::get_ready)
                        .service(generic::whoami)
                        .service(generic::get_static_assets)
                    )
            );

        if *SWAGGER_UI_EXTERNAL {
            app = app.service(swagger.clone());
        }

        app
    })
    // overwrites the number of worker threads -> default == available cpu cores
    .workers(workers)
    .shutdown_timeout(10);

    match listen_scheme {
        ListenScheme::Http => {
            server
                .bind(format!("{}:{}", &listen_addr, get_http_port()))?
                .run()
                .await
        }

        ListenScheme::Https => {
            server
                .bind_rustls_021(
                    format!("{}:{}", &listen_addr, get_https_port()),
                    tls::load_tls().await,
                )?
                .run()
                .await
        }

        ListenScheme::HttpHttps => {
            server
                .bind(format!("{}:{}", &listen_addr, get_http_port()))?
                .bind_rustls_021(
                    format!("{}:{}", &listen_addr, get_https_port()),
                    tls::load_tls().await,
                )?
                .run()
                .await
        }
    }
}

fn get_http_port() -> String {
    let port = env::var("LISTEN_PORT_HTTP").unwrap_or_else(|_| "8080".to_string());
    info!("HTTP listen port: {}", port);
    port
}

fn get_https_port() -> String {
    let port = env::var("LISTEN_PORT_HTTPS").unwrap_or_else(|_| "8443".to_string());
    info!("HTTPS listen port: {}", port);
    port
}

async fn TEMP_migrate_passkeys_uv(db: &DbPool) -> Result<(), ErrorResponse> {
    use rauthy_models::entity::webauthn::PasskeyEntity;
    use webauthn_rs::prelude::Credential;

    let entities: Vec<PasskeyEntity> = query_as!(
        PasskeyEntity,
        "select * from passkeys where user_verified is null"
    )
    .fetch_all(db)
    .await?;

    // TODO
    let mut count = 0;
    for entity in entities {
        let pk = entity.get_pk();
        let cred = Credential::from(pk.clone());
        let uv = Some(cred.user_verified);
        query!(
            "update passkeys set user_verified = $1 where passkey_user_id = $2",
            uv,
            entity.passkey_user_id
        )
        .execute(db)
        .await?;
        count += 1;
    }

    debug!("\n\n\tupdated {} passkey user_verified columns\n", count);

    Ok(())
}
