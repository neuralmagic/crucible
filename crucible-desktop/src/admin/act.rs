//! Carrying out one management action through crux.

use crate::admin::{Action, AdminView};
use crate::net::{Connection, within};
use crux::dto;

pub(in crate::admin) async fn act(
    c: &Connection,
    action: &Action,
    view: &mut AdminView,
) -> Result<(), String> {
    let client = &c.client;
    match action {
        Action::Enable { schedule, on } => within(
            "saving the schedule",
            client.update_schedule::<serde_json::Value>(&schedule.id, &schedule.body(*on)),
        )
        .await
        .map(drop),
        Action::Grant {
            of,
            grantee,
            role,
            not_after,
        } => within(
            "the share",
            client.grant_share(of, grantee, role, not_after.as_deref()),
        )
        .await
        .map(drop),
        Action::Revoke { of, grantee } => {
            within("the share", client.revoke_share(of, grantee)).await
        }
        Action::Rotate { secret, value } => {
            within("the rotation", client.rotate_secret(secret, value))
                .await
                .map(drop)
        }
        Action::Unbind { secret, binding } => {
            within("the binding", client.unbind_secret(secret, binding)).await
        }
        Action::Answer {
            id,
            labels,
            digest,
            note,
        } => within(
            "the answer",
            client.answer_decision(id, labels, digest, note.as_deref()),
        )
        .await
        .map(drop),
        Action::CreateSecret { body } => within(
            "the new secret",
            client.create_secret::<serde_json::Value>(body),
        )
        .await
        .map(drop),
        Action::MoveSecret { secret, owner } => {
            within("the owner change", client.transfer_secret(secret, owner))
                .await
                .map(drop)
        }
        Action::FireSchedule { id, .. } => {
            within("the firing", client.fire_schedule::<serde_json::Value>(id))
                .await
                .map(drop)
        }
        Action::TestFireSchedule { id, cursor, .. } => {
            within("the test firing", client.test_fire_schedule(id, cursor))
                .await
                .map(drop)
        }
        Action::SaveSchedule { id: None, body } => within(
            "the new schedule",
            client.create_schedule::<serde_json::Value>(body),
        )
        .await
        .map(drop),
        Action::SaveSchedule { id: Some(id), body } => within(
            "the schedule",
            client.update_schedule::<serde_json::Value>(id, body),
        )
        .await
        .map(drop),
        Action::RegisterImport {
            import,
            id,
            description,
        } => within(
            "the registration",
            client.register_import(import, &dto::RegisterImport { id, description }),
        )
        .await
        .map(drop),
        Action::DiscardImport { import } => within("the discard", client.discard_import(import))
            .await
            .map(drop),
        Action::CancelLaunch { key } => within("the cancel", client.cancel_launch(key))
            .await
            .map(drop),
        Action::MintKey { name, days } => within("the new key", client.mint_api_key(name, *days))
            .await
            .map(|key| view.minted = Some(key)),
        Action::RevokeKey { id } => within("the revoke", client.revoke_api_key(id)).await,
        Action::SignOutMcp { client: one } => {
            within("the sign-out", client.sign_out_mcp(one.as_deref()))
                .await
                .map(drop)
        }
        Action::CreateTeam { slug, name } => within("the new team", client.create_team(slug, name))
            .await
            .map(drop),
        Action::RenameTeam { slug, name } => within("the rename", client.rename_team(slug, name))
            .await
            .map(drop),
        Action::DeleteTeam { slug } => within("the delete", client.delete_team(slug)).await,
        Action::PutMembers { slug, members } => {
            within("the members", client.put_team_members(slug, members))
                .await
                .map(drop)
        }
        Action::EnableWebhook { id, on } => {
            within("the webhook", client.set_webhook_enabled(id, *on))
                .await
                .map(drop)
        }
        Action::RotateWebhook { id } => within("the rotation", client.rotate_webhook_secret(id))
            .await
            .map(|rotated| view.rotated = Some(rotated)),
        Action::DeleteWebhook { id } => within("the delete", client.delete_webhook(id)).await,
        Action::SaveWebhook { id: None, body } => {
            within("the new webhook", client.create_webhook(body))
                .await
                .map(|created| view.rotated = Some(created))
        }
        Action::SaveWebhook { id: Some(id), body } => {
            within("the webhook", client.update_webhook(id, body))
                .await
                .map(drop)
        }
        Action::SaveProvider { id: None, register } => {
            within("the registration", client.register_provider(register))
                .await
                .map(drop)
        }
        Action::SaveProvider {
            id: Some(id),
            register,
        } => within(
            "the provider",
            client.update_provider(id, &register.provider),
        )
        .await
        .map(drop),
        Action::DeleteProvider { id } => within("the delete", client.delete_provider(id)).await,
        Action::CreatePolicy { text } => within("the policy set", client.create_policy_set(text))
            .await
            .map(|stored| view.stored_policy = Some(stored.digest)),
        Action::ParkIssue { key, reason } => {
            within("the park", client.park::<serde_json::Value>(key, reason))
                .await
                .map(drop)
        }
        Action::UnparkIssue { key } => {
            within("the unpark", client.unpark::<serde_json::Value>(key, None))
                .await
                .map(drop)
        }
        Action::ActivatePolicy { digest } => {
            within("the activation", client.activate_policy_set(digest)).await
        }
        Action::Explain {
            login,
            action,
            resource,
        } => {
            view.explained = None;
            within(
                "the explanation",
                client.explain(login, action, resource.as_deref()),
            )
            .await
            .map(|explained| view.explained = Some(explained))
        }
        Action::SetOverrides {
            overrides,
            justification,
        } => within(
            "the overrides",
            client.set_overrides(overrides, justification),
        )
        .await
        .map(drop),
        Action::SetAutopilot { enabled, reason } => {
            within("the autopilot flag", client.set_autopilot(*enabled, reason))
                .await
                .map(drop)
        }
        Action::Reconcile => within("the reconcile", client.reconcile::<serde_json::Value>())
            .await
            .map(drop),
        Action::AddRepo {
            repo,
            justification,
        } => within("the new repo", client.add_repo(repo, justification)).await,
        Action::PauseRepo { repo, paused } => {
            within("the repo", client.pause_repo(repo, *paused)).await
        }
        Action::UnwatchRepo { repo } => within("the repo", client.unwatch_repo(repo)).await,
        Action::UseBlueprint {
            id,
            to,
            description,
        } => within(
            "the new draft",
            client.use_blueprint(id, to, description.as_deref()),
        )
        .await
        .map(|used| view.used = Some(used)),
        Action::CheckWebhook { body } => within("the check", client.check_webhook(body))
            .await
            .map(|check| view.webhook_check = Some(check)),
        Action::PreviewWebhook {
            body,
            sample,
            headers,
        } => within("the preview", client.preview_webhook(body, sample, headers))
            .await
            .map(|preview| view.webhook_preview = Some(preview)),
    }
}
