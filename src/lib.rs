//! # aws_sso
//!
//! A crate for managing AWS Single Sign-On (SSO) workflows.
//!
//! This crate provides utilities for retrieving AWS credentials for multiple accounts and roles via AWS SSO. It integrates with the AWS SSO OIDC workflow and fetches temporary credentials for accounts and roles assigned to a user.
//!
//! ## Requirements
//!
//! - AWS SSO must be enabled for your AWS organization.
//! - A valid AWS SSO start URL and region are required.
//! - The `~/.aws/credentials` file will be updated with the fetched credentials.
//! - Access to the device authorization page via a web browser.
//!
//! ## Examples
//!
//! To use the library, create an instance of [`AwsSsoWorkflow`] and call `run_workflow` to retrieve credentials:
//!
//! ```no_run
//! use aws_sso::AwsSsoWorkflow;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create a new AWS SSO workflow instance.
//!     let mut workflow = AwsSsoWorkflow::default();
//!
//!     // Run the workflow to fetch credentials.
//!     let credential = workflow.run_workflow().await?;
//!
//!     // Print the retrieved credentials.
//!     println!("Account ID: {}", credential.account_id);
//!     println!("Role Name: {}", credential.role_name);
//!     println!("Access Key ID: {}", credential.access_key_id);
//!     println!("Secret Access Key: {}", credential.secret_access_key);
//!     println!("Session Token: {}", credential.session_token);
//!     println!("---------------------------------");
//!
//!     Ok(())
//! }
//! ```
//!
//! ## `run_workflow` Details
//!
//! The `run_workflow` function handles the complete AWS SSO process, including:
//! - Registering an AWS SSO client.
//! - Starting the device authorization process.
//! - Polling for an authorization token.
//! - Fetching accounts and roles assigned to the authenticated user.
//! - Writing default AWS credentials to `~/.aws/credentials`.
//!
//! ### Side Effects
//! - Updates the local AWS credentials file (`~/.aws/credentials`) with fetched credentials.
//! - Opens the browser for device authorization.
//!
//! [`AwsSsoWorkflow`]: struct.AwsSsoWorkflow.html

use aws_config::BehaviorVersion;
use aws_sdk_sso::config::Region;
use aws_sdk_sso::Client as SsoClient;
use aws_sdk_ssooidc::operation::create_token::CreateTokenOutput;
use aws_sdk_ssooidc::Client as SsoOidcClient;
use skim::prelude::*;
use std::error::Error;
use std::fs;
use std::io::{Cursor, Write};
use tokio::time::{sleep, Duration};

#[derive(Default, Clone)]
pub struct AwsSsoWorkflow {
    pub start_url: String,
    pub region: String,
}

pub struct Credential {
    pub account_id: String,
    pub role_name: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: String,
}

impl AwsSsoWorkflow {
    fn write_default_aws_credentials(
        access_key_id: &str,
        secret_access_key: &str,
        session_token: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        dirs_next::home_dir()
                .map(|home| home.join(".aws/credentials"))
                .ok_or_else(|| "Could not locate home directory".into())
                .and_then(|credentials_path| {
                    credentials_path
                        .parent()
                        .map(fs::create_dir_all)
                        .transpose()
                        .map_err(|e| e.into())
                        .and_then(|_| {
                            std::fs::write(
                                &credentials_path,
                                format!(
                                    "[default]\naws_access_key_id = {}\naws_secret_access_key = {}\naws_session_token = {}\n",
                                    access_key_id, secret_access_key, session_token
                                ),
                            )
                            .map(|_| {
                                println!("Default credentials written to: {:?}", credentials_path);
                            })
                            .map_err(|e| e.into())
                        })
                })
    }

    async fn register_client(
        sso_oidc_client: &SsoOidcClient,
        client_name: &str,
        client_type: &str,
    ) -> Result<(String, String), Box<dyn Error>> {
        sso_oidc_client
            .register_client()
            .client_name(client_name)
            .client_type(client_type)
            .send()
            .await
            .map_err(|e| Box::new(e) as Box<dyn Error>)
            .and_then(|response| {
                let client_id = response.client_id().ok_or("Missing client_id")?;
                let client_secret = response.client_secret().ok_or("Missing client_secret")?;
                Ok((client_id.to_string(), client_secret.to_string()))
            })
    }

    async fn start_device_authorization(
        sso_oidc_client: &SsoOidcClient,
        client_id: &str,
        client_secret: &str,
        start_url: &str,
    ) -> Result<(String, String, String, String, i32), Box<dyn Error>> {
        sso_oidc_client
            .start_device_authorization()
            .client_id(client_id)
            .client_secret(client_secret)
            .start_url(start_url)
            .send()
            .await
            .map_err(|e| Box::new(e) as Box<dyn Error>)
            .and_then(|sda| {
                Ok((
                    sda.device_code().ok_or("Missing device_code")?.to_string(),
                    sda.user_code().ok_or("Missing user_code")?.to_string(),
                    sda.verification_uri()
                        .ok_or("Missing verification_uri")?
                        .to_string(),
                    sda.verification_uri_complete()
                        .ok_or("Missing verification_uri_complete")?
                        .to_string(),
                    sda.interval(),
                ))
            })
    }

    async fn poll_for_token(
        sso_oidc_client: &SsoOidcClient,
        client_id: &str,
        client_secret: &str,
        device_code: &str,
        interval: u64,
    ) -> Result<CreateTokenOutput, Box<dyn Error>> {
        loop {
            match sso_oidc_client
                .create_token()
                .client_id(client_id.to_string())
                .client_secret(client_secret.to_string())
                .grant_type("urn:ietf:params:oauth:grant-type:device_code")
                .device_code(device_code)
                .send()
                .await
            {
                Ok(tr) => {
                    println!("Token received successfully.");
                    return Ok(tr);
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("authorization_pending") {
                        println!("Authorization pending, retrying in {} seconds...", interval);
                        sleep(Duration::from_secs(interval)).await;
                    } else if msg.contains("slow_down") {
                        println!(
                            "Too many requests. Slowing down, retrying in {} seconds...",
                            interval + 5
                        );
                        sleep(Duration::from_secs(interval + 5)).await;
                    } else {
                        eprintln!("Error: CreateToken failed with message: {}", msg);
                        return Err(format!("CreateToken failed: {}", msg).into());
                    }
                }
            }
        }
    }

    async fn process_selected_accounts_and_roles(
        sso_client: &SsoClient,
        access_token: &str,
        selected_items: Vec<String>,
    ) -> Result<Credential, Box<dyn std::error::Error>> {
        for selected_output in selected_items {
            let parts: Vec<&str> = selected_output.split(" - ").collect();
            if parts.len() != 3 {
                eprintln!("Invalid selection format: {}", selected_output);
                continue;
            }

            let account_id = parts[0];
            let role_name = parts[2];

            println!(
                "Fetching credentials for Account ID: {}, Role: {}",
                account_id, role_name
            );

            let credentials_resp = sso_client
                .get_role_credentials()
                .account_id(account_id)
                .role_name(role_name)
                .access_token(access_token)
                .send()
                .await?;

            if let Some(credentials) = credentials_resp.role_credentials() {
                let access_key_id = credentials.access_key_id().unwrap_or("").to_string();
                let secret_access_key = credentials.secret_access_key().unwrap_or("").to_string();
                let session_token = credentials.session_token().unwrap_or("").to_string();

                let creds = Credential {
                    account_id: account_id.to_string(),
                    role_name: role_name.to_string(),
                    access_key_id: access_key_id.clone(),
                    secret_access_key: secret_access_key.clone(),
                    session_token: session_token.clone(),
                };

                println!(
                    "Credentials fetched for Account ID: {}, Role: {}",
                    account_id, role_name
                );

                // Optional: Write credentials to AWS config
                AwsSsoWorkflow::write_default_aws_credentials(
                    &access_key_id,
                    &secret_access_key,
                    &session_token,
                )?;

                return Ok(creds); // Return the first successfully fetched credentials
            } else {
                eprintln!(
                    "Failed to fetch credentials for Account ID: {}, Role: {}",
                    account_id, role_name
                );
            }
        }

        Err("No valid credentials found".into()) // Return an error if no credentials were fetched
    }

    fn perform_fuzzy_search(
        account_role_strings: &[String],
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        Skim::run_with(
            &SkimOptionsBuilder::default()
                .height(Some("20"))
                .multi(true)
                .prompt(Some("Select accounts and roles: "))
                .build()
                .unwrap(),
            Some(
                SkimItemReader::default().of_bufread(Cursor::new(account_role_strings.join("\n"))),
            ),
        )
        .map(|out| {
            out.selected_items
                .iter()
                .map(|item| item.output().to_string())
                .collect()
        })
        .ok_or_else(|| "No selection made.".into())
    }

    fn extract_access_token(
        token_response: &CreateTokenOutput,
    ) -> Result<&str, Box<dyn std::error::Error>> {
        token_response
            .access_token()
            .ok_or_else(|| "Missing access_token".into())
    }

    async fn fetch_accounts_and_roles(
        sso_client: &SsoClient,
        access_token: &str,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        println!("Fetching accounts and roles...");
        let accounts_resp = sso_client
            .list_accounts()
            .access_token(access_token)
            .send()
            .await?;

        let accounts = accounts_resp.account_list();
        if accounts.is_empty() {
            return Ok(Vec::new());
        }

        let mut account_role_strings = Vec::new();

        for account in accounts {
            if let Some(account_id) = account.account_id() {
                let account_name = account.account_name().unwrap_or("Unknown");

                let roles_resp = sso_client
                    .list_account_roles()
                    .account_id(account_id)
                    .access_token(access_token)
                    .send()
                    .await?;

                for role in roles_resp.role_list() {
                    if let Some(role_name) = role.role_name() {
                        account_role_strings
                            .push(format!("{} - {} - {}", account_id, account_name, role_name));
                    }
                }
            }
        }

        Ok(account_role_strings)
    }

    fn prompt_input(prompt: &str) -> Result<String, Box<dyn Error>> {
        print!("{}: ", prompt);
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        Ok(input.trim().to_string())
    }

    pub async fn run_workflow(&mut self) -> Result<Credential, Box<dyn Error>> {
        self.start_url = Self::prompt_input("Enter the AWS start URL")?;

        self.region = Self::prompt_input("Enter the AWS region")?;

        println!(
            "Running AWS workflow with URL: {} and region: {}",
            self.start_url, self.region
        );

        let config: aws_config::SdkConfig = aws_config::defaults(BehaviorVersion::v2024_03_28())
            .region(Region::new(self.region.clone()))
            .load()
            .await;

        let sso_oidc_client = SsoOidcClient::new(&config);

        let (client_id, client_secret) =
            Self::register_client(&sso_oidc_client, "my-rust-sso-client", "public").await?;

        let (device_code, user_code, verification_uri, verification_uri_complete, interval) =
            Self::start_device_authorization(
                &sso_oidc_client,
                &client_id,
                &client_secret,
                &self.start_url,
            )
            .await?;

        println!("Opening the verification page in your browser...");
        if webbrowser::open(&verification_uri_complete).is_ok() {
            println!("Browser successfully opened. Please authenticate to continue.");
        } else {
            println!(
                "Could not open the browser. Please go to: {}",
                verification_uri
            );
            println!("Enter the user code: {}", user_code);
        }

        println!("Please complete the authentication in your browser.");
        println!("Press Enter after you have completed the process...");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;

        let token_response = Self::poll_for_token(
            &sso_oidc_client,
            &client_id,
            &client_secret,
            &device_code,
            interval as u64,
        )
        .await?;

        let sso_client = SsoClient::new(&config);

        let access_token = Self::extract_access_token(&token_response)?;
        println!("Access token retrieved successfully.");

        let account_role_strings =
            Self::fetch_accounts_and_roles(&sso_client, access_token).await?;
        if account_role_strings.is_empty() {
            println!("No accounts or roles found.");
            return Err("No accounts or roles found".into());
        }

        let selected_items = Self::perform_fuzzy_search(&account_role_strings)?;
        if selected_items.is_empty() {
            println!("No accounts selected.");
            return Err("No accounts selected".into());
        }

        let credentials =
            Self::process_selected_accounts_and_roles(&sso_client, access_token, selected_items)
                .await?;

        Ok(credentials)
    }
}
