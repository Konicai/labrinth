use hyper::client::HttpConnector;
use hyper_tls::{native_tls, HttpsConnector};

pub async fn init_client() -> clickhouse::error::Result<clickhouse::Client> {
    let database = dotenvy::var("CLICKHOUSE_DATABASE").unwrap();

    let client = {
        let mut http_connector = HttpConnector::new();
        http_connector.enforce_http(false); // allow https URLs

        let tls_connector = native_tls::TlsConnector::builder().build().unwrap().into();
        let https_connector = HttpsConnector::from((http_connector, tls_connector));
        let hyper_client = hyper::client::Client::builder().build(https_connector);

        clickhouse::Client::with_http_client(hyper_client)
            .with_url(dotenvy::var("CLICKHOUSE_URL").unwrap())
            .with_user(dotenvy::var("CLICKHOUSE_USER").unwrap())
            .with_password(dotenvy::var("CLICKHOUSE_PASSWORD").unwrap())
    };

    client
        .query(&format!("CREATE DATABASE IF NOT EXISTS {database}"))
        .execute()
        .await?;

    client
        .query(&format!(
            "
            CREATE TABLE IF NOT EXISTS {database}.views
            (
                id UUID,
                recorded DateTime64(4),
                domain String,
                site_path String,

                user_id UInt64,
                project_id UInt64,

                ip IPv6,
                country String,
                user_agent String,
                headers Array(Tuple(String, String)),
            )
            ENGINE = MergeTree()
            PRIMARY KEY (id, recorded)
            "
        ))
        .execute()
        .await?;

    client
        .query(&format!(
            "
            CREATE TABLE IF NOT EXISTS {database}.downloads
            (
                id UUID,
                recorded DateTime64(4),
                domain String,
                site_path String,

                user_id UInt64,
                project_id UInt64,
                version_id UInt64,

                ip IPv6,
                country String,
                user_agent String,
                headers Array(Tuple(String, String)),
            )
            ENGINE = MergeTree()
            PRIMARY KEY (id, recorded)
            "
        ))
        .execute()
        .await?;

    client
        .query(&format!(
            "
            CREATE TABLE IF NOT EXISTS {database}.playtime
            (
                id UUID,
                recorded DateTime64(4),
                seconds UInt64,

                user_id UInt64,
                project_id UInt64,
                version_id UInt64,

                loader String,
                game_version String,
                parent UInt64,
            )
            ENGINE = MergeTree()
            PRIMARY KEY (id, recorded)
            "
        ))
        .execute()
        .await?;

    Ok(client.with_database(database))
}
