use std::sync::atomic::Ordering;

use crate::error::AppError;
use crate::state::{AppState, SegmentEvent, UserAccount};

impl AppState {
    fn generate_token() -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let tid = format!("{:?}", std::thread::current().id());
        let mut h = DefaultHasher::new();
        (nanos, &tid, "t1").hash(&mut h);
        format!("{:016x}", h.finish())
    }

    fn hash_pw(user: &str, pass: &str) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        format!("{}:{}:wt1", user, pass).hash(&mut h);
        format!("{:016x}", h.finish())
    }

    pub fn check_rate_limit(&self, ip: &str) -> Result<(), AppError> {
        let now = Self::epoch_secs();
        let map = self.inner.login_throttle.lock().unwrap();
        if let Some(&(failures, blocked_until)) = map.get(ip)
            && failures >= 5
            && now < blocked_until
        {
            return Err(AppError::BadInput(format!(
                "too many attempts! try again in {}s",
                blocked_until.saturating_sub(now)
            )));
        }
        Ok(())
    }

    pub fn record_login_failure(&self, ip: &str) {
        let mut map = self.inner.login_throttle.lock().unwrap();
        let entry = map.entry(ip.to_string()).or_insert((0, 0));
        entry.0 += 1;
        if entry.0 >= 5 {
            entry.1 = Self::epoch_secs() + 60;
        }
    }

    pub fn clear_login_throttle(&self, ip: &str) {
        self.inner.login_throttle.lock().unwrap().remove(ip);
    }

    pub async fn create_anonymous_session(&self) -> (String, u32, String) {
        let uid = self.inner.next_uid.fetch_add(1, Ordering::SeqCst);
        let anon_name = format!("anonymous#{}", uid);
        let token = Self::generate_token();
        let acc = UserAccount {
            user_id: uid,
            username: anon_name.clone(),
            original_anon_name: anon_name.clone(),
            password_hash: None,
            session_token: token.clone(),
            is_anonymous: true,
            protected_cells: 0,
        };

        {
            let mut users = self.inner.users.write().await;
            if users.len() <= uid as usize {
                users.resize(uid as usize + 1, None);
            }
            users[uid as usize] = Some(acc.clone());
        }

        self.inner
            .name_to_id
            .write()
            .await
            .insert(anon_name.clone(), uid);
        self.inner.sessions.write().await.insert(token.clone(), uid);
        let _ = self.inner.segment_tx.send(SegmentEvent::UserUpsert(acc));
        (anon_name, uid, token)
    }

    pub async fn reset_password(&self, username: &str) -> Result<(), AppError> {
        let uid = self
            .get_user_by_name(username)
            .await
            .filter(|u| !u.is_anonymous)
            .ok_or_else(|| AppError::BadInput("user not found".into()))?
            .user_id;

        let mut users = self.inner.users.write().await;
        let acc = users[uid as usize]
            .as_mut()
            .ok_or_else(|| AppError::BadInput("user not found".into()))?;
        acc.password_hash = None;
        let updated = acc.clone();
        drop(users);
        let _ = self
            .inner
            .segment_tx
            .send(SegmentEvent::UserUpsert(updated));
        Ok(())
    }

    pub async fn verify_user(&self, name: &str, pass: &str) -> bool {
        let acc = match self.get_user_by_name(name).await {
            Some(u) if !u.is_anonymous => u,
            _ => return false,
        };

        match &acc.password_hash {
            Some(hash) => *hash == Self::hash_pw(name, pass),
            None => {
                let new_hash = Self::hash_pw(name, pass);
                let mut users = self.inner.users.write().await;
                if let Some(Some(u)) = users.get_mut(acc.user_id as usize) {
                    u.password_hash = Some(new_hash);
                    let updated = u.clone();
                    drop(users);
                    let _ = self
                        .inner
                        .segment_tx
                        .send(SegmentEvent::UserUpsert(updated));
                }
                true
            }
        }
    }

    pub async fn upgrade_to_named(
        &self,
        anon_name: &str,
        new_username: &str,
        password: &str,
        token: &str,
    ) -> Result<UserAccount, AppError> {
        if self.get_user_by_name(new_username).await.is_some() {
            return Err(AppError::BadInput("username taken".into()));
        }
        let anon_acc = self
            .get_user_by_name(anon_name)
            .await
            .ok_or_else(|| AppError::BadInput("session not found".into()))?;
        if !anon_acc.is_anonymous {
            return Err(AppError::BadInput("already registered".into()));
        }

        let upgraded = UserAccount {
            user_id: anon_acc.user_id,
            username: new_username.to_string(),
            original_anon_name: anon_acc.original_anon_name.clone(),
            password_hash: Some(Self::hash_pw(new_username, password)),
            session_token: token.to_string(),
            is_anonymous: false,
            protected_cells: anon_acc.protected_cells,
        };

        {
            let mut name_map = self.inner.name_to_id.write().await;
            name_map.remove(anon_name);
            name_map.insert(new_username.to_string(), upgraded.user_id);
        }
        self.inner.users.write().await[upgraded.user_id as usize] = Some(upgraded.clone());
        self.inner
            .sessions
            .write()
            .await
            .insert(token.to_string(), upgraded.user_id);
        let _ = self
            .inner
            .segment_tx
            .send(SegmentEvent::UserUpsert(upgraded.clone()));
        Ok(upgraded)
    }
}
