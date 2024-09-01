// This file is part of Moonfire NVR, a security camera network video recorder.
// Copyright (C) 2022 The Moonfire NVR Authors; see AUTHORS and LICENSE.txt.
// SPDX-License-Identifier: GPL-v3.0-or-later WITH GPL-3.0-linking-exception.

//! User management: `/api/users/*`.

use base::{bail, err};
use http::{Method, Request, StatusCode};

use crate::json::{self, PutUsersResponse, UserSubset, UserWithId};

use super::{
    into_json_body, parse_json_body, plain_response, require_csrf_if_session, serve_json, Caller,
    ResponseResult, Service,
};

impl Service {
    pub(super) async fn users(
        &self,
        req: Request<hyper::body::Incoming>,
        caller: Caller,
    ) -> ResponseResult {
        match *req.method() {
            Method::GET | Method::HEAD => self.get_users(req, caller).await,
            Method::POST => self.post_users(req, caller).await,
            _ => Ok(plain_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "GET, HEAD, or POST expected",
            )),
        }
    }

    async fn get_users(
        &self,
        req: Request<hyper::body::Incoming>,
        caller: Caller,
    ) -> ResponseResult {
        if !caller.permissions.admin_users {
            bail!(Unauthenticated, msg("must have admin_users permission"));
        }
        let l = self.db.lock();
        let users = l
            .users_by_id()
            .iter()
            .map(|(&id, user)| UserWithId {
                id,
                user: UserSubset::from(user),
            })
            .collect();
        serve_json(&req, &json::GetUsersResponse { users })
    }

    async fn post_users(
        &self,
        req: Request<hyper::body::Incoming>,
        caller: Caller,
    ) -> ResponseResult {
        if !caller.permissions.admin_users {
            bail!(Unauthenticated, msg("must have admin_users permission"));
        }
        let (parts, b) = into_json_body(req).await?;
        let mut r: json::PutUsers = parse_json_body(&b)?;
        require_csrf_if_session(&caller, r.csrf)?;
        let username = r
            .user
            .username
            .take()
            .ok_or_else(|| err!(InvalidArgument, msg("username must be specified")))?;
        let mut change = db::UserChange::add_user(username.to_owned());
        if let Some(Some(pwd)) = r.user.password.take() {
            change.set_password(pwd.to_owned());
        }
        if let Some(preferences) = r.user.preferences.take() {
            change.config.preferences = preferences;
        }
        if let Some(permissions) = r.user.permissions.take() {
            change.permissions = permissions.into();
        }
        if r.user != Default::default() {
            bail!(Unimplemented, msg("unsupported user fields: {r:#?}"));
        }
        let mut l = self.db.lock();
        let user = l.apply_user_change(change)?;
        serve_json(&parts, &PutUsersResponse { id: user.id })
    }

    pub(super) async fn user(
        &self,
        req: Request<hyper::body::Incoming>,
        caller: Caller,
        id: i32,
    ) -> ResponseResult {
        match *req.method() {
            Method::GET | Method::HEAD => self.get_user(req, caller, id).await,
            Method::DELETE => self.delete_user(req, caller, id).await,
            Method::PATCH => self.patch_user(req, caller, id).await,
            _ => Ok(plain_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "GET, HEAD, DELETE, or PATCH expected",
            )),
        }
    }

    async fn get_user(
        &self,
        req: Request<hyper::body::Incoming>,
        caller: Caller,
        id: i32,
    ) -> ResponseResult {
        require_same_or_admin(&caller, id)?;
        let db = self.db.lock();
        let user = db
            .users_by_id()
            .get(&id)
            .ok_or_else(|| err!(NotFound, msg("can't find requested user")))?;
        serve_json(&req, &UserSubset::from(user))
    }

    async fn delete_user(
        &self,
        req: Request<hyper::body::Incoming>,
        caller: Caller,
        id: i32,
    ) -> ResponseResult {
        if !caller.permissions.admin_users {
            bail!(Unauthenticated, msg("must have admin_users permission"));
        }
        let (_parts, b) = into_json_body(req).await?;
        let r: json::DeleteUser = parse_json_body(&b)?;
        require_csrf_if_session(&caller, r.csrf)?;
        let mut l = self.db.lock();
        l.delete_user(id)?;
        Ok(plain_response(StatusCode::NO_CONTENT, &b""[..]))
    }

    async fn patch_user(
        &self,
        req: Request<hyper::body::Incoming>,
        caller: Caller,
        id: i32,
    ) -> ResponseResult {
        require_same_or_admin(&caller, id)?;
        let (_parts, b) = into_json_body(req).await?;
        let r: json::PostUser = parse_json_body(&b)?;
        let mut db = self.db.lock();
        let user = db
            .get_user_by_id_mut(id)
            .ok_or_else(|| err!(NotFound, msg("can't find requested user")))?;
        if r.update.as_ref().and_then(|u| u.password).is_some()
            && r.precondition.as_ref().and_then(|p| p.password).is_none()
            && !caller.permissions.admin_users
        {
            bail!(
                Unauthenticated,
                msg("to change password, must supply previous password or have admin_users permission")
            );
        }
        require_csrf_if_session(&caller, r.csrf)?;
        if let Some(mut precondition) = r.precondition {
            if matches!(precondition.disabled.take(), Some(d) if d != user.config.disabled) {
                bail!(FailedPrecondition, msg("disabled mismatch"));
            }
            if matches!(precondition.username.take(), Some(n) if n != user.username) {
                bail!(FailedPrecondition, msg("username mismatch"));
            }
            if matches!(precondition.preferences.take(), Some(ref p) if p != &user.config.preferences)
            {
                bail!(FailedPrecondition, msg("preferences mismatch"));
            }
            if let Some(p) = precondition.password.take() {
                if !user.check_password(p)? {
                    bail!(FailedPrecondition, msg("password mismatch")); // or Unauthenticated?
                }
            }
            if let Some(p) = precondition.permissions.take() {
                if user.permissions != db::Permissions::from(p) {
                    bail!(FailedPrecondition, msg("permissions mismatch"));
                }
            }

            // Safety valve in case something is added to UserSubset and forgotten here.
            if precondition != Default::default() {
                bail!(
                    Unimplemented,
                    msg("preconditions not supported: {precondition:#?}"),
                );
            }
        }
        if let Some(mut update) = r.update {
            let mut change = user.change();

            // First, set up updates which non-admins are allowed to perform on themselves.
            if let Some(preferences) = update.preferences.take() {
                change.config.preferences = preferences;
            }
            match update.password.take() {
                None => {}
                Some(None) => change.clear_password(),
                Some(Some(p)) => change.set_password(p.to_owned()),
            }

            // Requires admin_users if there's anything else.
            if update != Default::default() && !caller.permissions.admin_users {
                bail!(Unauthenticated, msg("must have admin_users permission"));
            }
            if let Some(d) = update.disabled.take() {
                change.config.disabled = d;
            }
            if let Some(n) = update.username.take() {
                change.username = n.to_string();
            }
            if let Some(permissions) = update.permissions.take() {
                change.permissions = permissions.into();
            }

            // Safety valve in case something is added to UserSubset and forgotten here.
            if update != Default::default() {
                bail!(Unimplemented, msg("updates not supported: {update:#?}"));
            }

            // Then apply all together.
            db.apply_user_change(change)?;
        }
        Ok(plain_response(StatusCode::NO_CONTENT, &b""[..]))
    }
}

fn require_same_or_admin(caller: &Caller, id: i32) -> Result<(), base::Error> {
    if caller.user.as_ref().map(|u| u.id) != Some(id) && !caller.permissions.admin_users {
        bail!(
            Unauthenticated,
            msg("must be authenticated as supplied user or have admin_users permission"),
        );
    }
    Ok(())
}
