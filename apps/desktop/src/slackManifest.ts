/**
 * The Slack app a host's Otter needs (D-203): one app per host, since Slack
 * shares an app's messages among every host connected with it. The same
 * manifest D-200 started with, named after the host, as JSON (indentation
 * doesn't matter when it is pasted).
 */
export function slackManifest(host: string): string {
  // Slack takes 35 characters for an app's name; its bot's name is plainer.
  const name = `Otter (${host.slice(0, 26)})`;
  const bot = `Otter-${host.replace(/[^A-Za-z0-9._-]+/g, "-")}`.slice(0, 80);
  return JSON.stringify(
    {
      display_information: { name, description: `Otter's tasks on ${host}, in Slack`.slice(0, 140) },
      features: {
        app_home: { home_tab_enabled: false, messages_tab_enabled: true, messages_tab_read_only_enabled: false },
        bot_user: { display_name: bot, always_online: true },
        slash_commands: [{ command: "/otter", description: "New task, or what needs me", usage_hint: "status | new what you want", should_escape: false }],
      },
      oauth_config: {
        scopes: { bot: ["chat:write", "im:write", "im:history", "groups:history", "reactions:write", "files:read", "commands"] },
      },
      settings: {
        event_subscriptions: { bot_events: ["message.im", "message.groups"] },
        interactivity: { is_enabled: true },
        socket_mode_enabled: true,
        org_deploy_enabled: false,
        token_rotation_enabled: false,
      },
    },
    null,
    2,
  );
}

/** The other hosts connected as the same Slack app as this one (D-203). */
export function sharing(
  me: { team?: string; bot_id?: string } | undefined,
  others: { host: string; slack?: { team?: string; bot_id?: string; state: string } }[],
): string[] {
  if (!me?.bot_id) return [];
  return others.filter((o) => o.slack?.bot_id === me.bot_id && o.slack?.team === me.team).map((o) => o.host);
}
