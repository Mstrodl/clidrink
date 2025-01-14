use http::status::StatusCode;
use http::Uri;
use isahc::{auth::Authentication, prelude::*, HttpClient, Request};
use rpassword::prompt_password;
use serde::{de, Deserialize, Serialize};
use serde_json;
use std::fmt;
use std::io::{Read, Write};
use std::ops::DerefMut;
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::sync::Mutex;
use url::Url;
use users::get_current_username;

pub struct API {
  token: Arc<Mutex<Option<String>>>,
  api_base_url: String,
  password_function: Arc<Mutex<Box<PasswordFunction>>>,
}

#[derive(Debug)]
pub enum APIError {
  Unauthorized,
  BadFormat,
  HTTPError(http::Error),
  IsahcError(isahc::Error),
  ServerError(Option<Uri>, String),
  LoginAborted,
}

#[derive(Deserialize, Debug, Clone)]
struct ErrorResponse {
  error: String,
}

#[derive(Deserialize, Debug, Clone)]
struct MessageResponse {
  message: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DrinkList {
  pub machines: Vec<Machine>,
  pub message: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Machine {
  pub display_name: String,
  pub id: u64,
  pub is_online: bool,
  pub name: String,
  pub slots: Vec<Slot>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Slot {
  pub active: bool,
  pub count: Option<u64>,
  pub empty: bool,
  pub item: Item,
  pub machine: u64,
  pub number: u8,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Item {
  pub id: u64,
  pub name: String,
  pub price: u64,
}

#[derive(Deserialize, Debug, Clone)]
struct User {
  preferred_username: String,
}

#[derive(Deserialize, Debug, Clone)]
struct CreditResponse {
  user: CreditUser,
}

#[allow(non_snake_case)]
#[derive(Deserialize, Debug, Clone)]
struct CreditUser {
  #[serde(deserialize_with = "number_string_deserializer")]
  drinkBalance: i64,
}

fn number_string_deserializer<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
  D: de::Deserializer<'de>,
{
  let number: String = Deserialize::deserialize(deserializer)?;
  match number.parse::<i64>() {
    Ok(res) => Ok(res),
    Err(e) => Err(de::Error::custom(format!(
      "Failed to deserialize i64: {}",
      e
    ))),
  }
}

#[derive(Serialize, Debug, Clone)]
struct DropRequest {
  machine: String,
  slot: u8,
}

#[allow(non_snake_case)]
#[derive(Deserialize, Debug, Clone)]
struct DropResponse {
  drinkBalance: i64,
  // message: String,
}

impl std::error::Error for APIError {}

impl fmt::Display for APIError {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    match self {
      APIError::Unauthorized => write!(
        f,
        "Unauthorized (Did your Kerberos ticket expire?: `kinit`)"
      ),
      APIError::BadFormat => write!(f, "BadFormat (The server sent data we didn't understand)"),
      APIError::ServerError(path, message) => write!(
        f,
        "ServerError for {}: {}",
        match path {
          Some(ref uri) => uri.to_string(),
          None => "<unknown>".to_string(),
        },
        message
      ),
      APIError::HTTPError(err) => write!(f, "HTTPError: {}", err),
      APIError::IsahcError(err) => write!(f, "IsahcError: {}", err),
      APIError::LoginAborted => write!(f, "LoginAborted"),
    }
  }
}

impl Default for API {
  fn default() -> Self {
    Self::new(
      "https://drink.csh.rit.edu".to_string(),
      Box::new(API::default_password_prompt),
    )
  }
}

enum APIBody<T: Serialize> {
  Json(T),
  NoBody,
}

type TryPasswordFn = dyn Fn(String) -> Result<PasswordResult, APIError> + Send + 'static;
type PasswordFunction = dyn Fn(String, Box<TryPasswordFn>) + Send + 'static;

pub struct PasswordResult {
  pub message: String,
  pub success: bool,
}

impl<T: Serialize> From<APIBody<T>> for isahc::Body {
  fn from(body: APIBody<T>) -> Self {
    match body {
      APIBody::Json(value) => serde_json::to_string(&value).unwrap().into(),
      APIBody::NoBody => ().into(),
    }
  }
}

impl Clone for API {
  fn clone(&self) -> Self {
    Self {
      token: Arc::clone(&self.token),
      api_base_url: self.api_base_url.clone(),
      password_function: Arc::clone(&self.password_function),
    }
  }
}

impl API {
  pub fn new(api_base_url: String, password_function: Box<PasswordFunction>) -> API {
    // We should find a way to spin this off in a thread
    // api.get_token().ok();
    API {
      token: Arc::new(Mutex::new(None)),
      api_base_url,
      password_function: Arc::new(Mutex::new(password_function)),
    }
  }
  fn authenticated_request<O, I>(
    &self,
    builder: http::request::Builder,
    input: APIBody<I>,
  ) -> Result<O, APIError>
  where
    I: Serialize,
    O: de::DeserializeOwned,
  {
    let client = HttpClient::new().map_err(APIError::IsahcError)?;
    let token = self.get_token()?;
    let builder = builder
      .header("Authorization", token)
      .header("Accept", "application/json");
    let builder = match input {
      APIBody::Json(_) => builder.header("Content-Type", "application/json"),
      APIBody::NoBody => builder,
    };
    let mut response = client
      .send(builder.body(input).map_err(APIError::HTTPError)?)
      .map_err(APIError::IsahcError)?;
    match response.status() {
      StatusCode::OK => match response.json::<O>() {
        Ok(value) => Ok(value),
        Err(_) => Err(APIError::BadFormat),
      },
      _ => {
        let text = response.text().map_err(|_| APIError::BadFormat)?;
        let text_ref = &text;
        Err(APIError::ServerError(
          response.effective_uri().cloned(),
          serde_json::from_str::<ErrorResponse>(&text)
            .map(|body| body.error)
            .or_else(move |_| {
              serde_json::from_str::<MessageResponse>(text_ref).map(|body| body.message)
            })
            .unwrap_or(text),
        ))
      }
    }
  }
  pub fn drop(&self, machine: String, slot: u8) -> Result<i64, APIError> {
    self
      .authenticated_request::<DropResponse, _>(
        Request::post(format!("{}/drinks/drop", self.api_base_url)),
        APIBody::Json(DropRequest { machine, slot }),
      )
      .map(|drop| drop.drinkBalance)
  }

  fn take_token(&self, token: &mut Option<String>) -> Result<String, APIError> {
    match token {
      Some(token) => Ok(token.to_string()),
      None => {
        let response = Request::get("https://sso.csh.rit.edu/auth/realms/csh/protocol/openid-connect/auth?client_id=clidrink&redirect_uri=drink%3A%2F%2Fcallback&response_type=token%20id_token&scope=openid%20profile%20drink_balance&state=&nonce=")
          .authentication(Authentication::negotiate())
          .body(()).map_err(APIError::HTTPError)?.send().map_err(APIError::IsahcError)?;
        let location = match response.headers().get("Location") {
          Some(location) => location,
          None => {
            self.login()?;
            return self.take_token(token);
          }
        };
        let url = Url::parse(
          &location
            .to_str()
            .map_err(|_| APIError::BadFormat)?
            .replace('#', "?"),
        )
        .map_err(|_| APIError::BadFormat)?;

        for (key, value) in url.query_pairs() {
          if key == "access_token" {
            let value = format!("Bearer {}", value);
            *token = Some(value.clone());
            return Ok(value);
          }
        }
        Err(APIError::BadFormat)
      }
    }
  }

  pub fn get_token(&self) -> Result<String, APIError> {
    let mut token = self.token.lock().unwrap();
    self.take_token(token.deref_mut())
  }

  pub fn default_password_prompt(username: String, try_password: Box<TryPasswordFn>) {
    loop {
      let password = prompt_password(format!("Password for {username}: ")).unwrap();
      match (try_password)(password) {
        Ok(PasswordResult {
          success: false,
          message,
        }) => {
          print!("Login failed: {message}");
        }
        Ok(PasswordResult {
          success: true,
          message: _,
        }) => {
          return;
        }
        Err(_) => {
          return;
        }
      }
    }
  }

  pub fn set_password_prompt(&mut self, prompt: Box<PasswordFunction>) {
    self.password_function = Arc::new(Mutex::new(prompt));
  }

  fn login(&self) -> Result<(), APIError> {
    // Get credentials
    let username: String = std::env::var("CLINK_USERNAME")
      .ok()
      .or_else(|| get_current_username().and_then(|username| username.into_string().ok()))
      .or_else(|| std::env::var("USER").ok())
      .expect("Couldn't determine username");

    let password_function = self.password_function.lock().unwrap();
    let (tx_password, rx_password) = channel();
    // Get password
    (password_function)(
      username.clone(),
      Box::new(move |password| {
        // Start kinit, ready to get password from pipe
        let mut process = Command::new("kinit")
          .arg(format!("{}@CSH.RIT.EDU", username))
          .stdin(Stdio::piped())
          .stdout(Stdio::null())
          .stderr(Stdio::piped())
          .spawn()
          .unwrap();
        process
          .stdin
          .as_ref()
          .unwrap()
          .write_all(password.as_bytes())
          .unwrap();
        let success = process.wait().unwrap().success();
        if success {
          tx_password.send(()).unwrap();
        }
        let mut output = "".to_string();
        process.stderr.unwrap().read_to_string(&mut output).unwrap();
        Ok(PasswordResult {
          success,
          message: output,
        })
      }),
    );
    match rx_password.recv() {
      Ok(_) => Ok(()),
      Err(_) => Err(APIError::LoginAborted),
    }
  }

  pub fn get_credits(&self) -> Result<i64, APIError> {
    // Can also be used to get other user information
    let user: User = self.authenticated_request(
      Request::get("https://sso.csh.rit.edu/auth/realms/csh/protocol/openid-connect/userinfo"),
      APIBody::NoBody as APIBody<serde_json::Value>,
    )?;
    let credit_response: CreditResponse = self.authenticated_request(
      Request::get(format!(
        "{}/users/credits?uid={}",
        self.api_base_url, user.preferred_username
      )),
      APIBody::NoBody as APIBody<serde_json::Value>,
    )?;
    Ok(credit_response.user.drinkBalance)
  }

  pub fn get_status_for_machine(&self, machine: Option<&str>) -> Result<DrinkList, APIError> {
    self.authenticated_request(
      Request::get(format!(
        "{}/drinks{}",
        self.api_base_url,
        match machine {
          Some(machine) => format!("?machine={}", machine),
          None => "".to_string(),
        }
      )),
      APIBody::NoBody as APIBody<serde_json::Value>,
    )
  }
}
