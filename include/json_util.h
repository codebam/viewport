/* SPDX-License-Identifier: MIT
 *
 * Type-checked reads out of a JsonObject.
 *
 * json-glib answers "is this member here?" and "does it hold what I asked
 * for?" separately, and only the first question has an obvious function to
 * call. json_object_has_member() is true for {"type":5}, and the
 * json_object_get_string_member() that follows it logs a Json-CRITICAL and
 * hands back NULL — which the caller then walks into. Two crashes reachable
 * from a single postMessage() have already been fixed that way, one call site
 * at a time.
 *
 * These accessors answer both questions at once: a member that is absent, or
 * present with the wrong type, is reported the same way and never reaches the
 * caller as a NULL or a zero it did not ask for. They log nothing — a
 * malformed message is news for whoever sent it, and the caller is the one
 * that knows how to say so.
 */
#ifndef VIEWPORT_JSON_UTIL_H
#define VIEWPORT_JSON_UTIL_H

#include <stdbool.h>

#include <json-glib/json-glib.h>

bool viewport_json_int(JsonObject *object, const char *name, int *out);
bool viewport_json_double(JsonObject *object, const char *name, double *out);
bool viewport_json_bool(JsonObject *object, const char *name, bool *out);
const char *viewport_json_string(JsonObject *object, const char *name);
JsonObject *viewport_json_object(JsonObject *object, const char *name);

#endif
