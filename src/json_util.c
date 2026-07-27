/* SPDX-License-Identifier: MIT
 *
 * Type-checked reads out of a JsonObject. See include/json_util.h.
 */
#include "json_util.h"

/* Every accessor tolerates a NULL object, because the usual way to reach one
 * is viewport_json_object() on a member that turned out not to be an object —
 * and a caller that had to test for that before every read would be back to
 * writing the check it came here to avoid. */
static JsonNode *value_node(JsonObject *object, const char *name)
{
	if (object == NULL) {
		return NULL;
	}

	JsonNode *node = json_object_get_member(object, name);
	if (node == NULL || !JSON_NODE_HOLDS_VALUE(node)) {
		return NULL;
	}
	return node;
}

bool viewport_json_int(JsonObject *object, const char *name, int *out)
{
	JsonNode *node = value_node(object, name);
	if (node == NULL || json_node_get_value_type(node) != G_TYPE_INT64) {
		return false;
	}

	*out = (int)json_node_get_int(node);
	return true;
}

bool viewport_json_double(JsonObject *object, const char *name, double *out)
{
	JsonNode *node = value_node(object, name);
	if (node == NULL) {
		return false;
	}

	/* A whole number is stored as an int64 whatever the sender meant by it:
	 * JSON has one number type and json-glib picks the narrower of the two on
	 * the way in. Refusing an int64 here would reject {"scale":1} — a value a
	 * shell has every reason to send — while accepting {"scale":1.0}. */
	GType type = json_node_get_value_type(node);
	if (type == G_TYPE_INT64) {
		*out = (double)json_node_get_int(node);
		return true;
	}
	if (type == G_TYPE_DOUBLE) {
		*out = json_node_get_double(node);
		return true;
	}
	return false;
}

bool viewport_json_bool(JsonObject *object, const char *name, bool *out)
{
	JsonNode *node = value_node(object, name);
	if (node == NULL || json_node_get_value_type(node) != G_TYPE_BOOLEAN) {
		return false;
	}

	*out = json_node_get_boolean(node);
	return true;
}

const char *viewport_json_string(JsonObject *object, const char *name)
{
	JsonNode *node = value_node(object, name);
	if (node == NULL || json_node_get_value_type(node) != G_TYPE_STRING) {
		return NULL;
	}

	return json_node_get_string(node);
}

JsonObject *viewport_json_object(JsonObject *object, const char *name)
{
	if (object == NULL) {
		return NULL;
	}

	JsonNode *node = json_object_get_member(object, name);
	if (node == NULL || !JSON_NODE_HOLDS_OBJECT(node)) {
		return NULL;
	}
	return json_node_get_object(node);
}
