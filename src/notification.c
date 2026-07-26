/* SPDX-License-Identifier: MIT
 *
 * Notifications.
 *
 * Nothing on a Linux desktop sends a notification to the compositor. They go
 * over D-Bus, to whichever program has claimed org.freedesktop.Notifications —
 * usually mako or dunst, which then draw their own windows. That works, but it
 * means notification styling is a second configuration language in a second
 * program, and the notifications themselves are ordinary layer-shell surfaces
 * the shell knows nothing about.
 *
 * Here the compositor claims the name itself and forwards each notification to
 * the shell, which draws it as part of the desktop. The styling is the
 * stylesheet already open in the editor, the layout is the one already being
 * used, and a notification can sit inside the shell's own idea of the screen
 * rather than floating over it as a separate client.
 *
 * The D-Bus interface is small but exacting: identifiers must be reusable so a
 * program can replace its own previous notification rather than stacking
 * duplicates, actions must be answered by name, and closing must say why. All
 * of that is here; what a notification looks like is entirely the shell's.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>
#include <string.h>

#include <gio/gio.h>

#include <wlr/util/log.h>

#include "viewport.h"

#define NOTIFICATION_BUS_NAME "org.freedesktop.Notifications"
#define NOTIFICATION_OBJECT_PATH "/org/freedesktop/Notifications"

/* Why a notification went away. The values are the specification's. */
enum notification_closed_reason {
	NOTIFICATION_CLOSED_EXPIRED = 1,
	NOTIFICATION_CLOSED_DISMISSED = 2,
	NOTIFICATION_CLOSED_REQUESTED = 3,
};

struct viewport_notifications {
	struct viewport_server *server;
	GDBusConnection *connection;
	guint owner_id;
	guint registration_id;
	/* Identifiers are handed out by us and must never be zero: zero means "a
	 * new notification" in the Notify call, so reusing it as an id would make a
	 * replacement request indistinguishable from a fresh one. */
	guint32 next_id;
};

static const char introspection_xml[] =
	"<node>"
	"  <interface name='org.freedesktop.Notifications'>"
	"    <method name='Notify'>"
	"      <arg type='s' name='app_name' direction='in'/>"
	"      <arg type='u' name='replaces_id' direction='in'/>"
	"      <arg type='s' name='app_icon' direction='in'/>"
	"      <arg type='s' name='summary' direction='in'/>"
	"      <arg type='s' name='body' direction='in'/>"
	"      <arg type='as' name='actions' direction='in'/>"
	"      <arg type='a{sv}' name='hints' direction='in'/>"
	"      <arg type='i' name='expire_timeout' direction='in'/>"
	"      <arg type='u' name='id' direction='out'/>"
	"    </method>"
	"    <method name='CloseNotification'>"
	"      <arg type='u' name='id' direction='in'/>"
	"    </method>"
	"    <method name='GetCapabilities'>"
	"      <arg type='as' name='capabilities' direction='out'/>"
	"    </method>"
	"    <method name='GetServerInformation'>"
	"      <arg type='s' name='name' direction='out'/>"
	"      <arg type='s' name='vendor' direction='out'/>"
	"      <arg type='s' name='version' direction='out'/>"
	"      <arg type='s' name='spec_version' direction='out'/>"
	"    </method>"
	"    <signal name='NotificationClosed'>"
	"      <arg type='u' name='id'/>"
	"      <arg type='u' name='reason'/>"
	"    </signal>"
	"    <signal name='ActionInvoked'>"
	"      <arg type='u' name='id'/>"
	"      <arg type='s' name='action_key'/>"
	"    </signal>"
	"  </interface>"
	"</node>";

static GDBusNodeInfo *introspection_data;

/* Urgency lives in the hints rather than in an argument of its own. Low, normal
 * and critical are 0, 1 and 2; anything else is treated as normal. Critical
 * matters because the specification says such a notification must not expire on
 * its own, and the shell needs to know that. */
static guint8 urgency_from_hints(GVariant *hints)
{
	GVariant *value = g_variant_lookup_value(hints, "urgency", NULL);
	if (value == NULL) {
		return 1;
	}

	guint8 urgency = 1;
	if (g_variant_is_of_type(value, G_VARIANT_TYPE_BYTE)) {
		urgency = g_variant_get_byte(value);
	}
	g_variant_unref(value);
	return urgency > 2 ? 1 : urgency;
}

static void handle_method(GDBusConnection *connection, const char *sender,
	const char *object_path, const char *interface_name,
	const char *method_name, GVariant *parameters,
	GDBusMethodInvocation *invocation, gpointer user_data)
{
	struct viewport_notifications *notifications = user_data;

	if (strcmp(method_name, "GetServerInformation") == 0) {
		g_dbus_method_invocation_return_value(invocation,
			g_variant_new("(ssss)", "viewport", "viewport", "1", "1.2"));
		return;
	}

	if (strcmp(method_name, "GetCapabilities") == 0) {
		/* Only what the shell actually honours. Claiming more — sound, image
		 * data, markup this shell does not render — would have applications
		 * send content that is silently dropped. */
		const char *const capabilities[] = { "body", "actions", "persistence",
			NULL };
		g_dbus_method_invocation_return_value(invocation,
			g_variant_new("(^as)", capabilities));
		return;
	}

	if (strcmp(method_name, "CloseNotification") == 0) {
		guint32 id = 0;
		g_variant_get(parameters, "(u)", &id);

		viewport_ipc_notify_notification_closed(notifications->server, id);
		viewport_notifications_closed(notifications, id,
			NOTIFICATION_CLOSED_REQUESTED);
		g_dbus_method_invocation_return_value(invocation, NULL);
		return;
	}

	if (strcmp(method_name, "Notify") != 0) {
		g_dbus_method_invocation_return_error(invocation, G_DBUS_ERROR,
			G_DBUS_ERROR_UNKNOWN_METHOD, "unknown method %s", method_name);
		return;
	}

	const char *app_name = NULL, *app_icon = NULL, *summary = NULL,
		*body = NULL;
	guint32 replaces_id = 0;
	gint32 expire_timeout = -1;
	GVariantIter *actions_iter = NULL;
	GVariant *hints = NULL;

	g_variant_get(parameters, "(&su&s&s&sas@a{sv}i)", &app_name, &replaces_id,
		&app_icon, &summary, &body, &actions_iter, &hints, &expire_timeout);

	/* A replacement keeps the identifier it is replacing, which is how an
	 * application updates a progress notification in place instead of leaving a
	 * trail of them. */
	guint32 id = replaces_id != 0 ? replaces_id : notifications->next_id++;
	if (notifications->next_id == 0) {
		notifications->next_id = 1;
	}

	/* Actions arrive as a flat list of key, label, key, label. The pairs are
	 * what the shell shows as buttons; the key is what gets sent back. */
	GPtrArray *action_keys = g_ptr_array_new();
	GPtrArray *action_labels = g_ptr_array_new();
	const char *item = NULL;
	while (actions_iter != NULL &&
			g_variant_iter_next(actions_iter, "&s", &item)) {
		const char *label = NULL;
		if (!g_variant_iter_next(actions_iter, "&s", &label)) {
			break;
		}
		g_ptr_array_add(action_keys, (gpointer)item);
		g_ptr_array_add(action_labels, (gpointer)label);
	}

	viewport_ipc_notify_notification(notifications->server, id, app_name,
		app_icon, summary, body, urgency_from_hints(hints), expire_timeout,
		(const char *const *)action_keys->pdata,
		(const char *const *)action_labels->pdata, action_keys->len);

	g_ptr_array_free(action_keys, TRUE);
	g_ptr_array_free(action_labels, TRUE);
	if (actions_iter != NULL) {
		g_variant_iter_free(actions_iter);
	}
	g_variant_unref(hints);

	g_dbus_method_invocation_return_value(invocation, g_variant_new("(u)", id));
}

static const GDBusInterfaceVTable interface_vtable = {
	.method_call = handle_method,
};

void viewport_notifications_closed(struct viewport_notifications *notifications,
	uint32_t id, uint32_t reason)
{
	if (notifications == NULL || notifications->connection == NULL) {
		return;
	}

	/* The sending application is told, because a notification daemon that never
	 * reports closure leaves programs believing their notification is still on
	 * screen — and some wait for it before sending the next. */
	g_dbus_connection_emit_signal(notifications->connection, NULL,
		NOTIFICATION_OBJECT_PATH, NOTIFICATION_BUS_NAME, "NotificationClosed",
		g_variant_new("(uu)", id, reason), NULL);
}

void viewport_notifications_dismissed(
	struct viewport_notifications *notifications, uint32_t id)
{
	viewport_notifications_closed(notifications, id,
		NOTIFICATION_CLOSED_DISMISSED);
}

void viewport_notifications_expired(
	struct viewport_notifications *notifications, uint32_t id)
{
	viewport_notifications_closed(notifications, id,
		NOTIFICATION_CLOSED_EXPIRED);
}

void viewport_notifications_action(struct viewport_notifications *notifications,
	uint32_t id, const char *action)
{
	if (notifications == NULL || notifications->connection == NULL ||
			action == NULL) {
		return;
	}

	g_dbus_connection_emit_signal(notifications->connection, NULL,
		NOTIFICATION_OBJECT_PATH, NOTIFICATION_BUS_NAME, "ActionInvoked",
		g_variant_new("(us)", id, action), NULL);

	/* The specification has the notification close when an action is taken
	 * unless it says otherwise, and every application assumes it. */
	viewport_notifications_closed(notifications, id,
		NOTIFICATION_CLOSED_DISMISSED);
}

static void handle_bus_acquired(GDBusConnection *connection, const char *name,
	gpointer user_data)
{
	struct viewport_notifications *notifications = user_data;
	GError *error = NULL;

	notifications->connection = connection;
	notifications->registration_id = g_dbus_connection_register_object(
		connection, NOTIFICATION_OBJECT_PATH, introspection_data->interfaces[0],
		&interface_vtable, notifications, NULL, &error);

	if (notifications->registration_id == 0) {
		wlr_log(WLR_ERROR, "notifications: %s",
			error ? error->message : "registration failed");
		g_clear_error(&error);
		return;
	}

	wlr_log(WLR_INFO, "notification service up");
}

static void handle_name_lost(GDBusConnection *connection, const char *name,
	gpointer user_data)
{
	/* Not fatal, and not unusual: mako or dunst may already be running, in
	 * which case they keep the name and draw the notifications themselves. */
	wlr_log(WLR_INFO,
		"another notification daemon holds the bus name; leaving it to it");
}

struct viewport_notifications *viewport_notifications_create(
	struct viewport_server *server)
{
	struct viewport_notifications *notifications =
		calloc(1, sizeof(*notifications));
	if (notifications == NULL) {
		return NULL;
	}
	notifications->server = server;
	notifications->next_id = 1;

	GError *error = NULL;
	if (introspection_data == NULL) {
		introspection_data = g_dbus_node_info_new_for_xml(introspection_xml,
			&error);
		if (introspection_data == NULL) {
			wlr_log(WLR_ERROR, "notifications introspection: %s",
				error ? error->message : "parse failed");
			g_clear_error(&error);
			free(notifications);
			return NULL;
		}
	}

	notifications->owner_id = g_bus_own_name(G_BUS_TYPE_SESSION,
		NOTIFICATION_BUS_NAME, G_BUS_NAME_OWNER_FLAGS_NONE, handle_bus_acquired,
		NULL, handle_name_lost, notifications, NULL);

	return notifications;
}

void viewport_notifications_destroy(
	struct viewport_notifications *notifications)
{
	if (notifications == NULL) {
		return;
	}
	if (notifications->registration_id != 0 &&
			notifications->connection != NULL) {
		g_dbus_connection_unregister_object(notifications->connection,
			notifications->registration_id);
	}
	if (notifications->owner_id != 0) {
		g_bus_unown_name(notifications->owner_id);
	}
	free(notifications);
}
