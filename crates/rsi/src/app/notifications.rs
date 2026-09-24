//! Notification system operations for App.

use super::App;

impl App {
    /// Push a new notification onto the queue.
    /// Aggregates same-kind notifications that arrive within 500ms.
    pub fn push_notification(
        &mut self,
        kind: crate::types::NotificationKind,
        priority: crate::types::NotificationPriority,
        message: String,
        session_id: Option<uuid::Uuid>,
    ) {
        use std::time::{Duration, Instant};

        let ttl = match priority {
            crate::types::NotificationPriority::Low => Duration::from_secs(5),
            crate::types::NotificationPriority::Medium => Duration::from_secs(5),
            crate::types::NotificationPriority::High => Duration::from_secs(10),
        };

        // Aggregation: if the most recent active notification has the same kind
        // and was created within 500ms, update its message with a count instead.
        if let Some(last) = self.notifications.back_mut() {
            if last.kind == kind
                && !last.dismissed
                && last.created_at.elapsed() < Duration::from_millis(500)
            {
                let count = if let Some(rest) = last.message.strip_prefix('(') {
                    rest.split(')')
                        .next()
                        .and_then(|n| n.parse::<u32>().ok())
                        .unwrap_or(1)
                } else {
                    1
                };
                last.message = format!("({}) {}", count + 1, message);
                last.created_at = Instant::now();
                return;
            }
        }

        let id = self.next_notification_id;
        self.next_notification_id += 1;

        self.notifications.push_back(crate::types::Notification {
            id,
            kind,
            message: message.clone(),
            priority,
            created_at: Instant::now(),
            ttl,
            session_id,
            dismissed: false,
        });
    }

    /// Push a medium-priority informational notification.
    pub fn notify(&mut self, message: impl Into<String>) {
        self.push_notification(
            crate::types::NotificationKind::Info,
            crate::types::NotificationPriority::Medium,
            message.into(),
            None,
        );
    }

    /// Push a medium-priority operation success notification.
    pub fn notify_success(&mut self, message: impl Into<String>) {
        self.push_notification(
            crate::types::NotificationKind::OperationSuccess,
            crate::types::NotificationPriority::Medium,
            message.into(),
            None,
        );
    }

    /// Push a high-priority error notification.
    pub fn notify_error(&mut self, message: impl Into<String>) {
        self.push_notification(
            crate::types::NotificationKind::OperationFailed,
            crate::types::NotificationPriority::High,
            message.into(),
            None,
        );
    }

    /// Remove expired notifications, moving them to history.
    /// Called each render frame from the event loop.
    /// Prune expired notifications and report whether the visible transient
    /// footer notification changed (including disappearing entirely).
    pub fn expire_notifications(&mut self) -> bool {
        // The previous frame may still show a just-expired item, so compare
        // the queue's visible candidate before TTL pruning with the live one
        // after pruning rather than filtering the old candidate by TTL first.
        let active_before = self.queued_transient_notification_id();
        let mut expired = Vec::new();
        self.notifications.retain(|n| {
            if n.dismissed || n.created_at.elapsed() >= n.ttl {
                expired.push(n.clone());
                false
            } else {
                true
            }
        });

        for n in expired {
            self.notification_history.push(n);
        }

        // Cap history at 100 entries.
        if self.notification_history.len() > 100 {
            let excess = self.notification_history.len() - 100;
            self.notification_history.drain(..excess);
        }

        active_before != self.active_transient_notification_id()
    }

    pub fn active_transient_notification_id(&self) -> Option<u64> {
        use crate::types::NotificationPriority;
        self.notifications.iter().rev().find_map(|notification| {
            (!notification.dismissed
                && notification.created_at.elapsed() < notification.ttl
                && notification.priority >= NotificationPriority::Medium)
                .then_some(notification.id)
        })
    }

    fn queued_transient_notification_id(&self) -> Option<u64> {
        use crate::types::NotificationPriority;
        self.notifications.iter().rev().find_map(|notification| {
            (!notification.dismissed && notification.priority >= NotificationPriority::Medium)
                .then_some(notification.id)
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn expiry_reports_visible_replacement_and_removal_with_mixed_priorities() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        let now = std::time::Instant::now();
        app.notifications.extend([
            crate::types::Notification {
                id: 1,
                kind: crate::types::NotificationKind::Info,
                message: "older medium".into(),
                priority: crate::types::NotificationPriority::Medium,
                created_at: now,
                ttl: std::time::Duration::from_secs(5),
                session_id: None,
                dismissed: false,
            },
            crate::types::Notification {
                id: 2,
                kind: crate::types::NotificationKind::Info,
                message: "low".into(),
                priority: crate::types::NotificationPriority::Low,
                created_at: now,
                ttl: std::time::Duration::from_secs(5),
                session_id: None,
                dismissed: false,
            },
            crate::types::Notification {
                id: 3,
                kind: crate::types::NotificationKind::OperationFailed,
                message: "new high".into(),
                priority: crate::types::NotificationPriority::High,
                created_at: now,
                ttl: std::time::Duration::from_secs(10),
                session_id: None,
                dismissed: false,
            },
        ]);
        let high_id = app.active_transient_notification_id();
        assert_eq!(high_id, Some(3));
        app.notifications
            .back_mut()
            .expect("high notification")
            .created_at -= std::time::Duration::from_secs(11);
        let changed = app.expire_notifications();
        let active_after = app.active_transient_notification_id();
        assert!(
            changed,
            "active before {high_id:?}, active after {active_after:?}"
        );
        assert_ne!(app.active_transient_notification_id(), high_id);
        assert!(app.active_transient_notification_id().is_some());

        for notification in &mut app.notifications {
            if notification.priority >= crate::types::NotificationPriority::Medium {
                notification.created_at -= std::time::Duration::from_secs(6);
            }
        }
        assert!(app.expire_notifications());
        assert_eq!(app.active_transient_notification_id(), None);
    }
}
