use hickory_resolver::{
    config::{NameServerConfig, ResolverConfig, ResolverOpts},
    error::ResolveError,
    AsyncResolver,
};
use reqwest::{
    header::{self, HeaderValue},
    Client, StatusCode,
};
use serde::{Deserialize, Serialize};
use std::{
    env,
    error::Error,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    process::exit,
    time,
};
use tokio::{task::JoinSet, time::sleep};

//Structure fields as determined by https://techdocs.akamai.com/linode-api/reference/get-domain-records
#[derive(Deserialize)]
pub struct Domains {
    pub data: Vec<Domain>,
}

#[derive(Deserialize)]
pub struct Domain {
    pub id: i32,
    pub domain: String,
}

#[derive(Deserialize)]
pub struct Records {
    pub data: Vec<Record>,
}

#[derive(Deserialize)]
pub struct Record {
    pub id: i32,
    pub r#type: String,
    pub name: String,
    pub target: String,
}

#[derive(Deserialize)]
pub struct TextRecordResult {
    pub id: i32,
}

#[derive(Serialize)]
pub struct TextRecordInsert {
    pub r#type: String,
    pub name: String,
    pub target: String,
}

impl TextRecordInsert {
    fn new(r#type: &str, name: &str, target: &str) -> Self {
        TextRecordInsert {
            r#type: r#type.to_owned(),
            name: name.to_owned(),
            target: target.to_owned(),
        }
    }
}

pub fn new_connection() -> Client {
    let mut headers = header::HeaderMap::new();

    let api_token = concat!("Bearer ", env!("API_KEY"));
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static(api_token));

    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("Unexpextedly failed to create connection")
}

pub async fn get_domain_info(
    connection: Client,
    domain_name: &str,
) -> Result<(String, String, i32), Box<dyn Error + Send + Sync>> {
    let domains: Domains = connection
        .get("https://api.linode.com/v4/domains")
        .send()
        .await?
        .json()
        .await?;

    for entry in domains.data {
        if domain_name == &entry.domain {
            return Ok(("".to_owned(), entry.domain, entry.id));
        } else if domain_name.ends_with(&format!(".{}", &entry.domain)) {
            let subdomain_count = domain_name.len() - entry.domain.len() - 1;
            let subdomain = domain_name
                .to_owned()
                .drain(0..subdomain_count)
                .as_str()
                .to_owned();
            return Ok((subdomain, entry.domain, entry.id));
        }
    }
    return Err("Failed to find domain")?;
}

pub async fn get_record_id(
    connection: Client,
    domain_id: i32,
    subdomain: &str,
    token: &str,
) -> Result<Option<i32>, Box<dyn Error + Send + Sync>> {
    let records: Records = connection
        .get(format!(
            "https://api.linode.com/v4/domains/{}/records",
            domain_id
        ))
        .send()
        .await?
        .json()
        .await?;

    let record_name = match subdomain {
        "" => "_acme-challenge".to_owned(),
        hostname => format!("_acme-challenge.{hostname}"),
    };

    for record in records.data {
        if record.r#type == "TXT" && record.name == record_name && record.target == token {
            return Ok(Some(record.id));
        }
    }
    return Ok(None);
}

async fn add_txt_record(
    connection: Client,
    domain_name: String,
    token: String,
) -> Result<(String, String, i32), Box<dyn Error + Send + Sync>> {
    let (subdomain, _base_domain, domain_id) =
        get_domain_info(connection.clone(), &domain_name).await?;

    let record = TextRecordInsert::new("TXT", &subdomain, &token);

    let resp = connection
        .post(format!(
            "https://api.linode.com/v4/domains/{domain_id}/records"
        ))
        .json(&record)
        .send()
        .await?;
    let entry: TextRecordResult = resp.json().await?;
    Ok((domain_name, token, entry.id))
}

pub async fn remove_txt_record(
    connection: Client,
    domain_id: i32,
    record_id: i32,
) -> Result<StatusCode, reqwest::Error> {
    let status = connection
        .delete(format!(
            "https://api.linode.com/v4/domains/{domain_id}/records/{record_id}"
        ))
        .send()
        .await?
        .status();
    Ok(status)
}

pub async fn text_record_exists(domain: String, text_value: String) -> Result<(), ResolveError> {
    let mut resolver_config = ResolverConfig::default();
    resolver_config.add_name_server(NameServerConfig::new(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(92, 123, 94, 2)), 53),
        hickory_resolver::config::Protocol::Udp,
    ));
    let resolver = AsyncResolver::tokio(resolver_config, ResolverOpts::default());
    let response = resolver.txt_lookup(domain).await?;
    for record in response.iter() {
        if record.to_string() == text_value {
            return Ok(());
        }
    }
    Err(ResolveError::from("Did not find text value"))
}

pub async fn wait_for_record_population(
    domain: String,
    value: String,
) -> Result<(String, String), Box<dyn Error + Send + Sync>> {
    //wait for record to populate, or give up after 20 minutes
    for _ in 0..80 {
        match text_record_exists(domain.to_owned(), value.to_owned()).await {
            Ok(_) => return Ok((domain, value)),
            Err(_) => (),
        }
        sleep(time::Duration::from_secs(15)).await;
    }
    Err("Record lookup timed out")?
}

async fn deploy_challenge(args: Vec<String>) -> Result<(), Box<dyn Error + Send + Sync>> {
    println!("**********************************************************************************");
    println!("Deploying TXT records for listed challenges:");

    //pair up Hostname/Value pairs for text records (toss token filenames as doing DNS)
    let challenges: Vec<[&String; 2]> = args.chunks(3).map(|x| [&x[0], &x[2]]).collect();

    let connection = new_connection();

    let mut deploy_set = JoinSet::new();
    let mut confirm_set = JoinSet::new();

    for [domain_name, token] in challenges {
        let target = format!("_acme-challenge.{}", domain_name);

        //deploy text records asynchronously
        deploy_set.spawn(add_txt_record(
            connection.clone(),
            target.to_owned(),
            token.to_owned(),
        ));

        //run dns lookup requests asynchronously
        confirm_set.spawn(wait_for_record_population(target, token.to_owned()));
    }

    while let Some(result) = deploy_set.join_next().await {
        //loop until all text records are added

        let (domain_name, token, id) = result??;

        println!("Added token '{token}' for '{domain_name}' - id:{id}")
    }

    println!("All records deployed. Please WAIT for Linode DNS to refresh");
    println!("This normally takes 2 minutes or so (extreme cases up to 20 minutes)");
    println!("...");

    while let Some(_) = confirm_set.join_next().await {}

    println!("All records confirmed as available");
    println!("**********************************************************************************");
    Ok(())
}

async fn clean_challenge(args: Vec<String>) -> Result<(), Box<dyn Error + Send + Sync>> {
    //pair up Hostname/Value pairs for text records (toss token filenames as doing DNS)
    let challenges: Vec<[&String; 2]> = args.chunks(3).map(|x| [&x[0], &x[2]]).collect();

    let connection = new_connection();
    for [domain_name, token] in challenges {
        let (subdomain, _base_domain, domain_id) =
            get_domain_info(connection.clone(), &domain_name).await?;

        match get_record_id(connection.clone(), domain_id, &subdomain, token).await? {
            Some(id) => _ = remove_txt_record(connection.clone(), domain_id, id).await?,
            None => (),
        };
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() > 1 {
        match args[1].as_str() {
            "deploy_challenge" => match deploy_challenge(args[2..].to_vec()).await {
                Ok(_) => exit(0),
                Err(_) => exit(1),
            },
            "clean_challenge" => match clean_challenge(args[2..].to_vec()).await {
                Ok(_) => exit(0),
                Err(_) => exit(1),
            },
            "sync_cert" => (), //Nothing implemented
            "deploy_cert" => {
                println!("**********************************************************************************");
                println!("Certificate created for {}", args[2]);
                println!("Certfile path: {}", args[4]);
                println!("**********************************************************************************");
            }
            "unchanged_cert" => {
                println!("**********************************************************************************");
                println!("Certificate for {} is already valid", args[2]);
                println!("Certfile path: {}", args[4]);
                println!("**********************************************************************************");
            }
            "invalid_challenge" => {
                println!("**********************************************************************************");
                println!(
                    "CHALLENGE FAILED FOR DOMAIN {} WITH RESPONSE {}",
                    args[2], args[3]
                );
                println!("**********************************************************************************");
            }
            "generate_csr" => (), //Nothing implemented
            "startup_hook" => (), //Nothing implemented
            "exit_hook" => {
                if args.len() > 2 {
                    println!("Process ended with errors: {}", args[2])
                }
            }
            _ => (), //Unknown argument, no message as specifically requested to ignore
        }
    }
}
