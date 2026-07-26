/* SPDX-License-Identifier: MIT
 *
 * Dark mode, via the desktop settings portal.
 *
 * A compositor cannot make its clients dark by styling anything: Firefox, GTK
 * and Qt apps each decide for themselves, and they all ask the same question —
 * the `color-scheme` key in the `org.freedesktop.appearance` namespace, read
 * over D-Bus through xdg-desktop-portal. Without something answering, every
 * application falls back to light regardless of how the shell looks.
 *
 * Normally that answer comes from a desktop environment. There isn't one here,
 * so the compositor provides it directly by implementing
 * org.freedesktop.impl.portal.Settings. This is deliberately not done by
 * setting GSettings keys: that route needs dconf and GNOME's schemas
 * installed, and silently does nothing when they are absent.
 *
 * Values follow the spec: 0 no preference, 1 prefer dark, 2 prefer light.
 */
#define _POSIX_C_SOURCE 200809L

#include <stdlib.h>
#include <string.h>

#include <gio/gio.h>

#include <wlr/util/log.h>

#include "viewport.h"

#define PORTAL_BUS_NAME "org.freedesktop.impl.portal.desktop.viewport"
#define PORTAL_OBJECT_PATH "/org/freedesktop/portal/desktop"
#define SETTINGS_INTERFACE "org.freedesktop.impl.portal.Settings"
#define APPEARANCE_NAMESPACE "org.freedesktop.appearance"
#define GNOME_NAMESPACE "org.gnome.desktop.interface"

struct viewport_appearance {
	struct viewport_server *server;
	GDBusConnection *connection;
	guint owner_id;
	guint registration_id;
	/* 1 = prefer dark, 2 = prefer light, 0 = no preference. */
	guint32 color_scheme;
};

static const char introspection_xml[] =
	"<node>"
	"  <interface name='" SETTINGS_INTERFACE "'>"
	"    <method name='ReadAll'>"
	"      <arg type='as' name='namespaces' direction='in'/>"
	"      <arg type='a{sa{sv}}' name='value' direction='out'/>"
	"    </method>"
	"    <method name='Read'>"
	"      <arg type='s' name='namespace' direction='in'/>"
	"      <arg type='s' name='key' direction='in'/>"
	"      <arg type='v' name='value' direction='out'/>"
	"    </method>"
	"    <signal name='SettingChanged'>"
	"      <arg type='s' name='namespace'/>"
	"      <arg type='s' name='key'/>"
	"      <arg type='v' name='value'/>"
	"    </signal>"
	"    <property name='version' type='u' access='read'/>"
	"  </interface>"
	"</node>";

static GDBusNodeInfo *introspection_data;

/* Font and scaling settings.
 *
 * Answering only color-scheme is worse than providing no portal at all: a
 * client that finds a working Settings implementation trusts it instead of
 * falling back to its own defaults, so a missing font or text-scaling value
 * becomes a measurement of zero rather than a sensible default. Toolkits size
 * menus from exactly these keys.
 *
 * The values are conventional defaults rather than anything clever — the point
 * is that every key a toolkit asks for gets a usable answer. */
static void add_gnome_settings(struct viewport_appearance *appearance,
	GVariantBuilder *builder)
{
	g_variant_builder_add(builder, "{sv}", "color-scheme",
		g_variant_new_string(appearance->color_scheme == 1
			? "prefer-dark" : "prefer-light"));
	g_variant_builder_add(builder, "{sv}", "gtk-theme",
		g_variant_new_string(appearance->color_scheme == 1
			? "Adwaita-dark" : "Adwaita"));
	g_variant_builder_add(builder, "{sv}", "icon-theme",
		g_variant_new_string("Adwaita"));
	g_variant_builder_add(builder, "{sv}", "font-name",
		g_variant_new_string("Sans 10"));
	g_variant_builder_add(builder, "{sv}", "monospace-font-name",
		g_variant_new_string("Monospace 10"));
	g_variant_builder_add(builder, "{sv}", "document-font-name",
		g_variant_new_string("Sans 10"));
	g_variant_builder_add(builder, "{sv}", "text-scaling-factor",
		g_variant_new_double(1.0));
	g_variant_builder_add(builder, "{sv}", "cursor-theme",
		g_variant_new_string("default"));
	g_variant_builder_add(builder, "{sv}", "cursor-size",
		g_variant_new_int32(24));
	g_variant_builder_add(builder, "{sv}", "enable-animations",
		g_variant_new_boolean(TRUE));
}

/* Returns a floating value, or NULL when the key is genuinely unknown. */
static GVariant *lookup_setting(struct viewport_appearance *appearance,
	const char *namespace, const char *key)
{
	if (strcmp(namespace, APPEARANCE_NAMESPACE) == 0) {
		if (strcmp(key, "color-scheme") == 0) {
			return g_variant_new_uint32(appearance->color_scheme);
		}
		/* Documented in the appearance namespace; clients read both. */
		if (strcmp(key, "contrast") == 0) {
			return g_variant_new_uint32(0);
		}
		return NULL;
	}

	if (strcmp(namespace, GNOME_NAMESPACE) != 0) {
		return NULL;
	}

	GVariantBuilder builder;
	g_variant_builder_init(&builder, G_VARIANT_TYPE("a{sv}"));
	add_gnome_settings(appearance, &builder);
	GVariant *all = g_variant_ref_sink(g_variant_builder_end(&builder));

	GVariant *value = g_variant_lookup_value(all, key, NULL);
	g_variant_unref(all);
	return value;
}

static void handle_method(GDBusConnection *connection, const char *sender,
	const char *object_path, const char *interface_name,
	const char *method_name, GVariant *parameters,
	GDBusMethodInvocation *invocation, gpointer user_data)
{
	struct viewport_appearance *appearance = user_data;

	if (strcmp(method_name, "ReadAll") == 0) {
		GVariantBuilder namespaces;
		g_variant_builder_init(&namespaces, G_VARIANT_TYPE("a{sa{sv}}"));

		GVariantBuilder appearance_keys;
		g_variant_builder_init(&appearance_keys, G_VARIANT_TYPE("a{sv}"));
		g_variant_builder_add(&appearance_keys, "{sv}", "color-scheme",
			g_variant_new_uint32(appearance->color_scheme));
		g_variant_builder_add(&namespaces, "{sa{sv}}", APPEARANCE_NAMESPACE,
			&appearance_keys);

		GVariantBuilder gnome_keys;
		g_variant_builder_init(&gnome_keys, G_VARIANT_TYPE("a{sv}"));
		add_gnome_settings(appearance, &gnome_keys);
		g_variant_builder_add(&namespaces, "{sa{sv}}", GNOME_NAMESPACE,
			&gnome_keys);

		g_dbus_method_invocation_return_value(invocation,
			g_variant_new("(a{sa{sv}})", &namespaces));
		return;
	}

	if (strcmp(method_name, "Read") == 0) {
		const char *namespace = NULL, *key = NULL;
		g_variant_get(parameters, "(&s&s)", &namespace, &key);

		GVariant *value = lookup_setting(appearance, namespace, key);
		if (value != NULL) {
			g_dbus_method_invocation_return_value(invocation,
				g_variant_new("(v)", value));
			return;
		}

		/* The spec requires this specific error for anything unknown, and
		 * clients rely on it to fall back cleanly. */
		g_dbus_method_invocation_return_error(invocation, G_DBUS_ERROR,
			G_DBUS_ERROR_UNKNOWN_PROPERTY, "unknown setting %s/%s", namespace,
			key);
		return;
	}

	g_dbus_method_invocation_return_error(invocation, G_DBUS_ERROR,
		G_DBUS_ERROR_UNKNOWN_METHOD, "unknown method %s", method_name);
}

static GVariant *handle_get_property(GDBusConnection *connection,
	const char *sender, const char *object_path, const char *interface_name,
	const char *property_name, GError **error, gpointer user_data)
{
	if (strcmp(property_name, "version") == 0) {
		return g_variant_new_uint32(2);
	}
	return NULL;
}

static const GDBusInterfaceVTable interface_vtable = {
	.method_call = handle_method,
	.get_property = handle_get_property,
};

static void handle_bus_acquired(GDBusConnection *connection, const char *name,
	gpointer user_data)
{
	struct viewport_appearance *appearance = user_data;
	GError *error = NULL;

	appearance->connection = connection;
	appearance->registration_id = g_dbus_connection_register_object(connection,
		PORTAL_OBJECT_PATH, introspection_data->interfaces[0],
		&interface_vtable, appearance, NULL, &error);

	if (appearance->registration_id == 0) {
		wlr_log(WLR_ERROR, "settings portal: %s",
			error ? error->message : "registration failed");
		g_clear_error(&error);
		return;
	}

	wlr_log(WLR_INFO, "settings portal up, color-scheme=%u",
		appearance->color_scheme);
}

static void handle_name_lost(GDBusConnection *connection, const char *name,
	gpointer user_data)
{
	/* Not fatal: without a session bus, or with another portal already owning
	 * the name, applications simply keep their own defaults. */
	wlr_log(WLR_INFO,
		"settings portal unavailable; clients will use their own defaults");
}

void viewport_appearance_set_dark(struct viewport_appearance *appearance,
	bool dark)
{
	if (appearance == NULL) {
		return;
	}

	guint32 value = dark ? 1 : 2;
	if (appearance->color_scheme == value) {
		return;
	}
	appearance->color_scheme = value;

	if (appearance->connection == NULL) {
		return;
	}

	/* Running applications switch on this signal rather than at next start. */
	g_dbus_connection_emit_signal(appearance->connection, NULL,
		PORTAL_OBJECT_PATH, SETTINGS_INTERFACE, "SettingChanged",
		g_variant_new("(ssv)", APPEARANCE_NAMESPACE, "color-scheme",
			g_variant_new_uint32(value)),
		NULL);

	wlr_log(WLR_INFO, "color-scheme now %s", dark ? "dark" : "light");
}

bool viewport_appearance_is_dark(struct viewport_appearance *appearance)
{
	return appearance != NULL && appearance->color_scheme == 1;
}

struct viewport_appearance *viewport_appearance_create(
	struct viewport_server *server, bool dark)
{
	struct viewport_appearance *appearance = calloc(1, sizeof(*appearance));
	if (appearance == NULL) {
		return NULL;
	}
	appearance->server = server;
	appearance->color_scheme = dark ? 1 : 2;

	GError *error = NULL;
	if (introspection_data == NULL) {
		introspection_data = g_dbus_node_info_new_for_xml(introspection_xml,
			&error);
		if (introspection_data == NULL) {
			wlr_log(WLR_ERROR, "settings portal introspection: %s",
				error ? error->message : "parse failed");
			g_clear_error(&error);
			free(appearance);
			return NULL;
		}
	}

	/* G_BUS_NAME_OWNER_FLAGS_REPLACE loses to a real desktop portal rather
	 * than fighting it, which matters if this is ever run inside a session
	 * that already provides one. */
	appearance->owner_id = g_bus_own_name(G_BUS_TYPE_SESSION, PORTAL_BUS_NAME,
		G_BUS_NAME_OWNER_FLAGS_NONE, handle_bus_acquired, NULL,
		handle_name_lost, appearance, NULL);

	return appearance;
}

void viewport_appearance_destroy(struct viewport_appearance *appearance)
{
	if (appearance == NULL) {
		return;
	}
	if (appearance->registration_id != 0 && appearance->connection != NULL) {
		g_dbus_connection_unregister_object(appearance->connection,
			appearance->registration_id);
	}
	if (appearance->owner_id != 0) {
		g_bus_unown_name(appearance->owner_id);
	}
	free(appearance);
}
