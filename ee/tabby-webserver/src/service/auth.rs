use std::sync::Arc;

use anyhow::{anyhow, Context};
use argon2::{
    password_hash,
    password_hash::{rand_core::OsRng, SaltString},
    Argon2, PasswordHasher, PasswordVerifier,
};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use juniper::ID;
use tabby_db::{DbConn, InvitationDAO};
use tokio::task::JoinHandle;
use tracing::warn;

use super::{graphql_pagination_to_filter, AsID, AsRowid};
use crate::{
    oauth,
    schema::{
        auth::{
            generate_jwt, generate_refresh_token, validate_jwt, AuthenticationService, Invitation,
            JWTPayload, OAuthCredential, OAuthError, OAuthProvider, OAuthResponse,
            RefreshTokenResponse, RegisterResponse, RequestInvitationInput, TokenAuthResponse,
            UpdateOAuthCredentialInput, User,
        },
        email::EmailService,
        license::{IsLicenseValid, LicenseService},
        setting::SettingService,
        CoreError, Result,
    },
};

#[derive(Clone)]
struct AuthenticationServiceImpl {
    db: DbConn,
    mail: Arc<dyn EmailService>,
    license: Arc<dyn LicenseService>,
}

pub fn new_authentication_service(
    db: DbConn,
    mail: Arc<dyn EmailService>,
    license: Arc<dyn LicenseService>,
) -> impl AuthenticationService {
    AuthenticationServiceImpl { db, mail, license }
}

#[async_trait]
impl AuthenticationService for AuthenticationServiceImpl {
    async fn register(
        &self,
        email: String,
        password: String,
        invitation_code: Option<String>,
    ) -> Result<RegisterResponse> {
        let is_admin_initialized = self.is_admin_initialized().await?;
        let invitation =
            check_invitation(&self.db, is_admin_initialized, invitation_code, &email).await?;

        // check if email exists
        if self.db.get_user_by_email(&email).await?.is_some() {
            return Err(anyhow!("Email is already registered").into());
        }

        let Ok(pwd_hash) = password_hash(&password) else {
            return Err(anyhow!("Unknown error").into());
        };

        let id = if let Some(invitation) = invitation {
            self.db
                .create_user_with_invitation(
                    email.clone(),
                    pwd_hash,
                    !is_admin_initialized,
                    invitation.id,
                )
                .await?
        } else {
            self.db
                .create_user(email.clone(), pwd_hash, !is_admin_initialized)
                .await?
        };

        let user = self.db.get_user(id).await?.unwrap();

        let refresh_token = generate_refresh_token();
        self.db.create_refresh_token(id, &refresh_token).await?;

        let Ok(access_token) = generate_jwt(JWTPayload::new(user.email.clone(), user.is_admin))
        else {
            return Err(anyhow!("Unknown error").into());
        };

        let resp = RegisterResponse::new(access_token, refresh_token);
        Ok(resp)
    }

    async fn allow_self_signup(&self) -> Result<bool> {
        let domain_list = self
            .db
            .read_security_setting()
            .await?
            .allowed_register_domain_list;
        let is_email_configured = self.mail.read_email_setting().await?.is_some();
        Ok(is_email_configured && !domain_list.is_empty())
    }

    async fn request_password_reset_email(&self, email: String) -> Result<Option<JoinHandle<()>>> {
        let user = self.get_user_by_email(&email).await.ok();

        let Some(user @ User { active: true, .. }) = user else {
            return Ok(None);
        };

        let id = user.id.as_rowid()?;
        let existing = self.db.get_password_reset_by_user_id(id as i64).await?;
        if let Some(existing) = existing {
            if Utc::now().signed_duration_since(*existing.created_at) < Duration::minutes(5) {
                return Err(anyhow!(
                    "A password reset has been requested recently, please try again later"
                )
                .into());
            }
        }
        let code = self.db.create_password_reset(id as i64).await?;
        let handle = self
            .mail
            .send_password_reset_email(user.email, code.clone())
            .await?;
        Ok(Some(handle))
    }

    async fn password_reset(&self, code: &str, password: &str) -> Result<()> {
        let password_encrypted = password_hash(password).map_err(|_| anyhow!("Unknown error"))?;

        let user_id = self.db.verify_password_reset(code).await?;
        self.db.delete_password_reset_by_user_id(user_id).await?;
        self.db
            .update_user_password(user_id as i32, password_encrypted)
            .await?;
        Ok(())
    }

    async fn token_auth(&self, email: String, password: String) -> Result<TokenAuthResponse> {
        let Some(user) = self.db.get_user_by_email(&email).await? else {
            return Err(anyhow!("User not found").into());
        };

        if !user.active {
            return Err(anyhow!("User is disabled").into());
        }

        if !password_verify(&password, &user.password_encrypted) {
            return Err(anyhow!("Password is not valid").into());
        }

        let refresh_token = generate_refresh_token();
        self.db
            .create_refresh_token(user.id, &refresh_token)
            .await?;

        let Ok(access_token) = generate_jwt(JWTPayload::new(user.email.clone(), user.is_admin))
        else {
            return Err(anyhow!("Unknown error").into());
        };

        let resp = TokenAuthResponse::new(access_token, refresh_token);
        Ok(resp)
    }

    async fn refresh_token(&self, token: String) -> Result<RefreshTokenResponse> {
        let Some(refresh_token) = self.db.get_refresh_token(&token).await? else {
            return Err(anyhow!("Invalid refresh token").into());
        };
        if refresh_token.is_expired() {
            return Err(anyhow!("Expired refresh token").into());
        }
        let Some(user) = self.db.get_user(refresh_token.user_id).await? else {
            return Err(anyhow!("User not found").into());
        };

        if !user.active {
            return Err(anyhow!("User is disabled").into());
        }

        let new_token = generate_refresh_token();
        self.db.replace_refresh_token(&token, &new_token).await?;

        // refresh token update is done, generate new access token based on user info
        let Ok(access_token) = generate_jwt(JWTPayload::new(user.email.clone(), user.is_admin))
        else {
            return Err(anyhow!("Unknown error").into());
        };

        let resp = RefreshTokenResponse::new(access_token, new_token, refresh_token.expires_at);

        Ok(resp)
    }

    async fn delete_expired_token(&self) -> Result<()> {
        self.db.delete_expired_token().await?;
        Ok(())
    }

    async fn delete_expired_password_resets(&self) -> Result<()> {
        self.db.delete_expired_password_resets().await?;
        Ok(())
    }

    async fn verify_access_token(&self, access_token: &str) -> Result<JWTPayload> {
        let claims = validate_jwt(access_token).map_err(anyhow::Error::new)?;
        Ok(claims)
    }

    async fn is_admin_initialized(&self) -> Result<bool> {
        let admin = self.db.list_admin_users().await?;
        Ok(!admin.is_empty())
    }

    async fn update_user_role(&self, id: &ID, is_admin: bool) -> Result<()> {
        let id = id.as_rowid()?;
        let user = self.db.get_user(id).await?.context("User doesn't exits")?;
        if user.is_owner() {
            return Err(anyhow!("The owner's admin status cannot be changed").into());
        }
        Ok(self.db.update_user_role(id, is_admin).await?)
    }

    async fn get_user_by_email(&self, email: &str) -> Result<User> {
        let user = self.db.get_user_by_email(email).await?;
        if let Some(user) = user {
            Ok(user.into())
        } else {
            Err(anyhow!("User not found {}", email).into())
        }
    }

    async fn create_invitation(&self, email: String) -> Result<Invitation> {
        if !self.license.read_license().await.is_license_valid() {
            return Err(CoreError::InvalidLicense(
                "This feature requires enterprise license",
            ));
        };

        let invitation = self.db.create_invitation(email.clone()).await?;
        let email_sent = self
            .mail
            .send_invitation_email(email, invitation.code.clone())
            .await;
        match email_sent {
            Ok(_) | Err(CoreError::EmailNotConfigured) => {}
            Err(e) => warn!(
                "Failed to send invitation email, please check your SMTP settings are correct: {e}"
            ),
        }
        Ok(invitation.into())
    }

    async fn request_invitation_email(&self, input: RequestInvitationInput) -> Result<Invitation> {
        if !self
            .db
            .read_security_setting()
            .await?
            .can_register_without_invitation(&input.email)
        {
            return Err(anyhow!("Your email does not belong to any known authentication domains. Please contact the administrator for assistance.").into());
        }
        let invitation = AuthenticationService::create_invitation(self, input.email).await?;
        Ok(invitation)
    }

    async fn delete_invitation(&self, id: &ID) -> Result<ID> {
        Ok(self.db.delete_invitation(id.as_rowid()?).await?.as_id())
    }

    async fn reset_user_auth_token(&self, email: &str) -> Result<()> {
        Ok(self.db.reset_user_auth_token_by_email(email).await?)
    }

    async fn list_users(
        &self,
        after: Option<String>,
        before: Option<String>,
        first: Option<usize>,
        last: Option<usize>,
    ) -> Result<Vec<User>> {
        let (skip_id, limit, backwards) = graphql_pagination_to_filter(after, before, first, last)?;

        Ok(self
            .db
            .list_users_with_filter(skip_id, limit, backwards)
            .await?
            .into_iter()
            .map(|x| x.into())
            .collect())
    }

    async fn list_invitations(
        &self,
        after: Option<String>,
        before: Option<String>,
        first: Option<usize>,
        last: Option<usize>,
    ) -> Result<Vec<Invitation>> {
        let (limit, skip_id, backwards) = graphql_pagination_to_filter(after, before, first, last)?;
        Ok(self
            .db
            .list_invitations_with_filter(limit, skip_id, backwards)
            .await?
            .into_iter()
            .map(|x| x.into())
            .collect())
    }

    async fn oauth(
        &self,
        code: String,
        provider: OAuthProvider,
    ) -> std::result::Result<OAuthResponse, OAuthError> {
        let client = oauth::new_oauth_client(provider, Arc::new(self.clone()));
        let email = client.fetch_user_email(code).await?;
        let (user_id, is_admin) = get_or_create_oauth_user(&self.db, &email).await?;

        let refresh_token = generate_refresh_token();
        self.db
            .create_refresh_token(user_id, &refresh_token)
            .await?;

        let access_token = generate_jwt(JWTPayload::new(email.clone(), is_admin))
            .map_err(|_| OAuthError::Unknown)?;

        let resp = OAuthResponse {
            access_token,
            refresh_token,
        };
        Ok(resp)
    }

    async fn read_oauth_credential(
        &self,
        provider: OAuthProvider,
    ) -> Result<Option<OAuthCredential>> {
        match provider {
            OAuthProvider::Github => Ok(self
                .db
                .read_github_oauth_credential()
                .await?
                .map(|val| val.into())),
            OAuthProvider::Google => Ok(self
                .db
                .read_google_oauth_credential()
                .await?
                .map(|val| val.into())),
        }
    }

    async fn oauth_callback_url(&self, provider: OAuthProvider) -> Result<String> {
        let external_url = self.db.read_network_setting().await?.external_url;
        let url = match provider {
            OAuthProvider::Github => external_url + "/oauth/callback/github",
            OAuthProvider::Google => external_url + "/oauth/callback/google",
        };
        Ok(url)
    }

    async fn update_oauth_credential(&self, input: UpdateOAuthCredentialInput) -> Result<()> {
        match input.provider {
            OAuthProvider::Github => Ok(self
                .db
                .update_github_oauth_credential(&input.client_id, input.client_secret.as_deref())
                .await?),
            OAuthProvider::Google => Ok(self
                .db
                .update_google_oauth_credential(&input.client_id, input.client_secret.as_deref())
                .await?),
        }
    }

    async fn delete_oauth_credential(&self, provider: OAuthProvider) -> Result<()> {
        let ret = match provider {
            OAuthProvider::Github => self.db.delete_github_oauth_credential().await,
            OAuthProvider::Google => self.db.delete_google_oauth_credential().await,
        };
        Ok(ret?)
    }

    async fn update_user_active(&self, id: &ID, active: bool) -> Result<()> {
        let id = id.as_rowid()?;
        let user = self.db.get_user(id).await?.context("User doesn't exits")?;
        if user.is_owner() {
            return Err(anyhow!("The owner's active status cannot be changed").into());
        }
        Ok(self.db.update_user_active(id, active).await?)
    }
}

async fn get_or_create_oauth_user(db: &DbConn, email: &str) -> Result<(i32, bool), OAuthError> {
    if let Some(user) = db.get_user_by_email(email).await? {
        return user
            .active
            .then_some((user.id, user.is_admin))
            .ok_or(OAuthError::UserDisabled);
    }
    if db
        .read_security_setting()
        .await
        .map_err(|x| OAuthError::Other(x.into()))?
        .can_register_without_invitation(email)
    {
        // it's ok to set password to empty string here, because
        // 1. both `register` & `token_auth` mutation will do input validation, so empty password won't be accepted
        // 2. `password_verify` will always return false for empty password hash read from user table
        // so user created here is only able to login by github oauth, normal login won't work
        Ok((
            db.create_user(email.to_owned(), "".to_owned(), false)
                .await?,
            false,
        ))
    } else {
        let Some(invitation) = db.get_invitation_by_email(email).await.ok().flatten() else {
            return Err(OAuthError::UserNotInvited);
        };
        // safe to create with empty password for same reasons above
        let id = db
            .create_user_with_invitation(email.to_owned(), "".to_owned(), false, invitation.id)
            .await?;
        let user = db.get_user(id).await?.unwrap();
        Ok((user.id, user.is_admin))
    }
}

async fn check_invitation(
    db: &DbConn,
    is_admin_initialized: bool,
    invitation_code: Option<String>,
    email: &str,
) -> Result<Option<InvitationDAO>> {
    if !is_admin_initialized {
        // Creating the admin user, no invitation required
        return Ok(None);
    }

    let err = Err(anyhow!("Invitation code is not valid").into());
    let Some(invitation_code) = invitation_code else {
        return err;
    };

    let Some(invitation) = db.get_invitation_by_code(&invitation_code).await? else {
        return err;
    };

    if invitation.email != email {
        return err;
    }

    Ok(Some(invitation))
}

fn password_hash(raw: &str) -> password_hash::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2.hash_password(raw.as_bytes(), &salt)?.to_string();

    Ok(hash)
}

fn password_verify(raw: &str, hash: &str) -> bool {
    if let Ok(parsed_hash) = argon2::PasswordHash::new(hash) {
        let argon2 = Argon2::default();
        argon2.verify_password(raw.as_bytes(), &parsed_hash).is_ok()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {

    struct MockLicenseService(LicenseStatus);

    #[async_trait]
    impl LicenseService for MockLicenseService {
        async fn read_license(&self) -> Result<Option<LicenseInfo>> {
            Ok(Some(LicenseInfo {
                r#type: crate::schema::license::LicenseType::Team,
                status: self.0.clone(),
                seats: 1,
                seats_used: 1,
                issued_at: Utc::now(),
                expires_at: Utc::now(),
            }))
        }

        async fn update_license(&self, _: String) -> Result<()> {
            Ok(())
        }
    }

    async fn test_authentication_service() -> AuthenticationServiceImpl {
        let db = DbConn::new_in_memory().await.unwrap();
        AuthenticationServiceImpl {
            db: db.clone(),
            mail: Arc::new(new_email_service(db).await.unwrap()),
            license: Arc::new(MockLicenseService(LicenseStatus::Ok)),
        }
    }

    async fn test_authentication_service_without_valid_license() -> AuthenticationServiceImpl {
        let db = DbConn::new_in_memory().await.unwrap();
        AuthenticationServiceImpl {
            db: db.clone(),
            mail: Arc::new(new_email_service(db).await.unwrap()),
            license: Arc::new(MockLicenseService(LicenseStatus::Expired)),
        }
    }

    async fn test_authentication_service_with_mail() -> (AuthenticationServiceImpl, TestEmailServer)
    {
        let db = DbConn::new_in_memory().await.unwrap();
        let smtp = TestEmailServer::start().await;
        let service = AuthenticationServiceImpl {
            db: db.clone(),
            mail: Arc::new(smtp.create_test_email_service(db).await),
            license: Arc::new(MockLicenseService(LicenseStatus::Ok)),
        };
        (service, smtp)
    }

    use assert_matches::assert_matches;
    use juniper_axum::relay::{self, Connection};
    use serial_test::serial;

    use super::*;
    use crate::{
        schema::license::{LicenseInfo, LicenseStatus},
        service::email::{new_email_service, testutils::TestEmailServer},
    };

    #[test]
    fn test_password_hash() {
        let raw = "12345678";
        let hash = password_hash(raw).unwrap();

        assert_eq!(hash.len(), 97);
        assert!(hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
    }

    #[test]
    fn test_password_verify() {
        let raw = "12345678";
        let hash = password_hash(raw).unwrap();

        assert!(password_verify(raw, &hash));
        assert!(!password_verify(raw, "invalid hash"));
    }

    static ADMIN_EMAIL: &str = "test@example.com";
    static ADMIN_PASSWORD: &str = "123456789$acR";

    async fn register_admin_user(service: &AuthenticationServiceImpl) -> RegisterResponse {
        service
            .register(ADMIN_EMAIL.to_owned(), ADMIN_PASSWORD.to_owned(), None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_auth_token() {
        let service = test_authentication_service().await;
        assert_matches!(
            service
                .token_auth(ADMIN_EMAIL.to_owned(), "12345678".to_owned())
                .await,
            Err(_)
        );

        register_admin_user(&service).await;

        assert_matches!(
            service
                .token_auth(ADMIN_EMAIL.to_owned(), "12345678".to_owned())
                .await,
            Err(_)
        );

        let resp1 = service
            .token_auth(ADMIN_EMAIL.to_owned(), ADMIN_PASSWORD.to_owned())
            .await
            .unwrap();
        let resp2 = service
            .token_auth(ADMIN_EMAIL.to_owned(), ADMIN_PASSWORD.to_owned())
            .await
            .unwrap();
        // each auth should generate a new refresh token
        assert_ne!(resp1.refresh_token, resp2.refresh_token);
    }

    #[tokio::test]
    async fn test_invitation_flow() {
        let service = test_authentication_service().await;

        assert!(!service.is_admin_initialized().await.unwrap());
        register_admin_user(&service).await;

        let email = "user@user.com";
        let password = "12345678dD^";

        service.create_invitation(email.to_owned()).await.unwrap();
        let invitation = &service
            .list_invitations(None, None, None, None)
            .await
            .unwrap()[0];

        // Admin initialized, registeration requires a invitation code;
        assert_matches!(
            service
                .register(email.to_owned(), password.to_owned(), None)
                .await,
            Err(_)
        );

        // Invalid invitation code won't work.
        assert_matches!(
            service
                .register(
                    email.to_owned(),
                    password.to_owned(),
                    Some("abc".to_owned())
                )
                .await,
            Err(_)
        );

        // Register success.
        assert!(service
            .register(
                email.to_owned(),
                password.to_owned(),
                Some(invitation.code.clone()),
            )
            .await
            .is_ok());

        // Try register again with same email failed.
        assert_matches!(
            service
                .register(
                    email.to_owned(),
                    password.to_owned(),
                    Some(invitation.code.clone())
                )
                .await,
            Err(_)
        );

        // Used invitation should have been deleted,  following delete attempt should fail.
        assert!(service
            .db
            .delete_invitation(invitation.id.as_rowid().unwrap())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_refresh_token() {
        let service = test_authentication_service().await;
        let reg = register_admin_user(&service).await;

        let resp1 = service
            .refresh_token(reg.refresh_token.clone())
            .await
            .unwrap();
        // new access token should be valid
        assert!(validate_jwt(&resp1.access_token).is_ok());
        // refresh token should be renewed
        assert_ne!(reg.refresh_token, resp1.refresh_token);

        let resp2 = service
            .refresh_token(resp1.refresh_token.clone())
            .await
            .unwrap();
        // expire time should be no change
        assert_eq!(resp1.refresh_expires_at, resp2.refresh_expires_at);
    }

    #[tokio::test]
    async fn test_reset_user_auth_token() {
        let service = test_authentication_service().await;
        register_admin_user(&service).await;

        let user = service.get_user_by_email(ADMIN_EMAIL).await.unwrap();
        service.reset_user_auth_token(&user.email).await.unwrap();

        let user2 = service.get_user_by_email(ADMIN_EMAIL).await.unwrap();
        assert_ne!(user.auth_token, user2.auth_token);
    }

    #[tokio::test]
    async fn test_is_admin_initialized() {
        let service = test_authentication_service().await;

        assert!(!service.is_admin_initialized().await.unwrap());
        tabby_db::testutils::create_user(&service.db).await;
        assert!(service.is_admin_initialized().await.unwrap());
    }

    async fn list_users(
        db: &AuthenticationServiceImpl,
        after: Option<String>,
        before: Option<String>,
        first: Option<i32>,
        last: Option<i32>,
    ) -> Connection<User> {
        relay::query_async(
            after,
            before,
            first,
            last,
            |after, before, first, last| async move {
                Ok(db.list_users(after, before, first, last).await.unwrap())
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn test_request_invitation() {
        let service = test_authentication_service().await;
        service
            .db
            .update_security_setting(Some("example.com".into()), false)
            .await
            .unwrap();

        assert!(service
            .request_invitation_email(RequestInvitationInput {
                email: "test@example.com".into()
            })
            .await
            .is_ok());

        assert!(service
            .request_invitation_email(RequestInvitationInput {
                email: "test@gmail.com".into()
            })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_get_or_create_oauth_user() {
        let service = test_authentication_service().await;
        let id = service
            .db
            .create_user("test@example.com".into(), "".into(), false)
            .await
            .unwrap();
        service.db.update_user_active(id, false).await.unwrap();

        assert!(get_or_create_oauth_user(&service.db, "test@example.com")
            .await
            .is_err());

        service
            .db
            .update_security_setting(Some("example.com".into()), false)
            .await
            .unwrap();

        assert!(get_or_create_oauth_user(&service.db, "example@example.com")
            .await
            .is_ok());
        assert!(get_or_create_oauth_user(&service.db, "example@gmail.com")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_update_role() {
        let service = test_authentication_service().await;
        let _ = service
            .db
            .create_user("admin@example.com".into(), "".into(), true)
            .await
            .unwrap();

        let user_id = service
            .db
            .create_user("user@example.com".into(), "".into(), false)
            .await
            .unwrap();

        assert!(service
            .update_user_role(&user_id.as_id(), true)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_owner_status() {
        let service = test_authentication_service().await;
        let admin_id = service
            .db
            .create_user("admin@example.com".into(), "".into(), true)
            .await
            .unwrap();

        assert!(service
            .update_user_role(&admin_id.as_id(), false)
            .await
            .is_err());

        assert!(service
            .update_user_active(&admin_id.as_id(), false)
            .await
            .is_err());
    }

    #[tokio::test]
    #[serial]
    async fn test_password_reset() {
        let (service, smtp) = test_authentication_service_with_mail().await;

        // Test first reset, ensure wrong code fails
        service
            .db
            .create_user("user@example.com".into(), "pass".into(), true)
            .await
            .unwrap();
        let user = service.get_user_by_email("user@example.com").await.unwrap();

        let handle = service
            .request_password_reset_email("user@example.com".into())
            .await
            .unwrap();
        handle.unwrap().await.unwrap();
        assert!(smtp.list_mail().await[0]
            .subject
            .to_lowercase()
            .contains("password"));

        let reset = service
            .db
            .get_password_reset_by_user_id(user.id.as_rowid().unwrap() as i64)
            .await
            .unwrap()
            .unwrap();

        assert!(service.password_reset("", "newpass").await.is_err());
        assert!(service.password_reset(&reset.code, "newpass").await.is_ok());

        // Test second reset, ensure expired code fails
        let user = service
            .db
            .get_user_by_email(&user.email)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(user.password_encrypted, "pass");

        service
            .request_password_reset_email("user@example.com".into())
            .await
            .unwrap();
        let reset = service
            .db
            .get_password_reset_by_user_id(user.id as i64)
            .await
            .unwrap()
            .unwrap();

        service
            .db
            .mark_password_reset_expired(&reset.code)
            .await
            .unwrap();

        assert!(service
            .password_reset(&reset.code, "newpass2")
            .await
            .is_err());

        // Test third reset, ensure inactive users cannot reset their password
        let user_id_2 = service
            .db
            .create_user("user2@example.com".into(), "pass".into(), false)
            .await
            .unwrap();

        service
            .request_password_reset_email("user2@example.com".into())
            .await
            .unwrap();
        let reset = service
            .db
            .get_password_reset_by_user_id(user_id_2 as i64)
            .await
            .unwrap()
            .unwrap();

        service
            .db
            .update_user_active(user_id_2, false)
            .await
            .unwrap();

        assert!(service
            .password_reset(&reset.code, "newpass")
            .await
            .is_err());

        service
            .db
            .mark_password_reset_expired(&reset.code)
            .await
            .unwrap();
        service.delete_expired_password_resets().await.unwrap();
        assert!(service
            .db
            .get_password_reset_by_code(&reset.code)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_pagination() {
        let service = test_authentication_service().await;
        service
            .db
            .create_user("a@example.com".into(), "pass".into(), false)
            .await
            .unwrap();
        service
            .db
            .create_user("b@example.com".into(), "pass".into(), false)
            .await
            .unwrap();
        service
            .db
            .create_user("c@example.com".into(), "pass".into(), false)
            .await
            .unwrap();

        let all_users = list_users(&service, None, None, None, None).await;

        assert!(!all_users.page_info.has_next_page);
        assert!(!all_users.page_info.has_previous_page);

        let users = list_users(
            &service,
            Some(all_users.edges[0].cursor.clone()),
            None,
            None,
            None,
        )
        .await;

        assert!(!users.page_info.has_next_page);
        assert!(users.page_info.has_previous_page);

        let users = list_users(&service, None, None, Some(2), None).await;

        assert!(users.page_info.has_next_page);
        assert!(!users.page_info.has_previous_page);

        let users = list_users(
            &service,
            None,
            Some(all_users.edges[1].cursor.clone()),
            None,
            Some(1),
        )
        .await;

        assert!(users.page_info.has_next_page);
        assert!(!users.page_info.has_previous_page);

        let users = list_users(
            &service,
            Some(all_users.edges[2].cursor.clone()),
            None,
            None,
            None,
        )
        .await;
        assert!(!users.page_info.has_next_page);
        assert!(users.page_info.has_previous_page);

        let users = list_users(&service, None, None, Some(3), None).await;
        assert!(!users.page_info.has_next_page);
        assert!(!users.page_info.has_previous_page);

        let users = list_users(
            &service,
            Some(all_users.edges[0].cursor.clone()),
            None,
            Some(2),
            None,
        )
        .await;
        assert!(!users.page_info.has_next_page);
        assert!(users.page_info.has_previous_page);
    }

    #[tokio::test]
    #[serial]
    async fn test_allow_self_signup() {
        let (service, _) = test_authentication_service_with_mail().await;

        assert!(!service.allow_self_signup().await.unwrap());

        service
            .db
            .update_security_setting(Some("abc.com".to_owned()), false)
            .await
            .unwrap();

        assert!(service.allow_self_signup().await.unwrap());
    }

    #[tokio::test]
    async fn test_create_invitation_without_license() {
        let service = test_authentication_service_without_valid_license().await;
        assert_matches!(
            service.create_invitation("abc.com".into()).await,
            Err(CoreError::InvalidLicense(_))
        )
    }
}
