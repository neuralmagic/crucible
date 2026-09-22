// The default policy set of RFC-0003 C-POLICY. Every policy carries an @id; the decision's reason
// names the ids that decided. Resources carry `owner`, `owner_kind`, and, when the subject holds
// a role in the owner, `owner_role`; principals carry a tag per principal they act as.

@id("platform-admin-all")
permit(principal, action, resource)
when { principal.platform_admin };

@id("user-owner-all")
permit(principal, action, resource)
when { resource.owner_kind == "user" && resource has owner_role && resource.owner_role == "owner" };

@id("group-owner-all")
permit(principal, action, resource)
when { resource.owner_kind == "group" && resource has owner_role };

@id("team-member-read-launch")
permit(principal, action in [Action::"read", Action::"launch", Action::"dispatch"], resource)
when { resource.owner_kind == "team" && resource has owner_role };

@id("team-maintainer-write")
permit(principal, action in [Action::"create", Action::"update", Action::"bind", Action::"rotate"], resource)
when {
    resource.owner_kind == "team" && resource has owner_role
    && ["maintainer", "owner"].contains(resource.owner_role)
};

@id("team-owner-all")
permit(principal, action, resource)
when { resource.owner_kind == "team" && resource has owner_role && resource.owner_role == "owner" };

@id("everyone-read-platform")
permit(principal is UserPrincipal, action in [Action::"platform:read", Action::"policy_set:read", Action::"team:read"], resource)
when { resource.owner == "team:platform-administrators" };

@id("everyone-dispatch-platform-targets")
permit(principal is UserPrincipal, action == Action::"dispatch_target:dispatch", resource)
when { resource.owner == "team:platform-administrators" };

@id("everyone-read-platform-providers")
permit(principal is UserPrincipal, action == Action::"model_provider:read", resource)
when { resource.owner == "team:platform-administrators" };

@id("user-create-team")
permit(principal is UserPrincipal, action == Action::"team:create", resource);

@id("platform-operator-curation")
permit(principal is UserPrincipal, action == Action::"issue:update", resource)
when { principal.hasTag("team:platform-operators") && resource.owner == "team:platform-administrators" };

@id("share-viewer")
permit(principal, action in [Action::"read"], resource)
when { resource has share && ["viewer", "launcher", "editor"].contains(resource.share) };

@id("share-launcher")
permit(principal, action in [Action::"launch"], resource)
when { resource has share && ["launcher", "editor"].contains(resource.share) };

@id("share-editor")
permit(principal, action in [Action::"update", Action::"bind"], resource)
when { resource has share && resource.share == "editor" };

@id("run-read-own-tree")
permit(principal is RunPrincipal, action in [Action::"read"], resource)
when { resource has run && resource.run == principal.id };

@id("run-publish-self")
permit(principal is RunPrincipal, action == Action::"run:publish", resource)
when { resource has run && resource.run == principal.id };
