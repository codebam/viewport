/* SPDX-License-Identifier: MIT
 *
 * Unit tests for the pure logic that malformed input reaches first.
 *
 * Every other test in this tree starts a real headless compositor and drives a
 * Wayland client against it. That is the only way to judge presentation, but
 * it costs a two-minute timeout, a seat, a renderer and a display — and none of
 * it reaches the small functions where the crashes actually were. The two IPC
 * crashes fixed so far were both a type confusion inside a single accessor:
 * reachable from one postMessage(), invisible to a test that can only look at
 * pixels.
 *
 * So this covers src/json_util.c and nothing else. Those accessors are the
 * gate every field of every IPC message passes through, they are pure, and
 * they link against json-glib alone — which is what lets this binary run in
 * milliseconds on a hosted runner where the compositor tests cannot start at
 * all.
 *
 * src/config.c's viewport_output_config_for() was considered and left out. The
 * function itself is pure, but it lives in a file whose other half calls into
 * bindings, idle, input, appearance, IPC and wlr_log, so linking it drags in
 * wlroots and most of the compositor. Testing it would mean either splitting
 * config.c or building a fake server, and a fake server proves things about
 * the fake.
 *
 * Prints one line per case in the same shape as tests/shell.test.js and exits
 * non-zero if any of them failed.
 */
#include <stdbool.h>
#include <stdio.h>
#include <string.h>

#include <json-glib/json-glib.h>

#include "json_util.h"

static int failures;

static void check(const char *label, bool cond)
{
	printf("%s %s\n", cond ? "ok  " : "FAIL", label);
	if (!cond) {
		failures++;
	}
}

/* One object holding a member of every JSON type, so each accessor can be
 * pointed at all six and asked to accept exactly one of them. */
static const char *const document =
	"{"
	"  \"an_int\": 7,"
	"  \"a_double\": 1.5,"
	"  \"a_whole\": 1,"
	"  \"a_bool\": true,"
	"  \"a_string\": \"hello\","
	"  \"an_object\": { \"inner\": 3 },"
	"  \"an_array\": [1, 2],"
	"  \"a_null\": null"
	"}";

/* Absent from `document`, and named so that a typo in a test cannot make it
 * present by accident. */
static const char *const missing = "no_such_member";

static void test_int(JsonObject *root)
{
	/* The sentinel is the real assertion in the failure cases: an accessor
	 * that returns false having already written to *out is no safer than the
	 * json-glib getter it replaced, because the caller that trusted the
	 * return value still ends up using the value. */
	int out = -12345;

	check("int: reads an int", viewport_json_int(root, "an_int", &out));
	check("int: reads the right int", out == 7);

	out = -12345;
	check("int: rejects a string",
		!viewport_json_int(root, "a_string", &out));
	check("int: rejects a double",
		!viewport_json_int(root, "a_double", &out));
	check("int: rejects a bool", !viewport_json_int(root, "a_bool", &out));
	check("int: rejects an object",
		!viewport_json_int(root, "an_object", &out));
	check("int: rejects an array",
		!viewport_json_int(root, "an_array", &out));
	check("int: rejects null", !viewport_json_int(root, "a_null", &out));
	check("int: rejects an absent member",
		!viewport_json_int(root, missing, &out));
	check("int: rejects a NULL object",
		!viewport_json_int(NULL, "an_int", &out));
	check("int: leaves the output alone when it refuses", out == -12345);
}

static void test_double(JsonObject *root)
{
	double out = -12345.0;

	check("double: reads a double",
		viewport_json_double(root, "a_double", &out));
	check("double: reads the right double", out == 1.5);

	/* {"scale": 1} and {"scale": 1.0} are the same number to whoever sent
	 * them, and json-glib stores the first as an int64. A double accessor that
	 * only accepted G_TYPE_DOUBLE would reject the spelling a shell is most
	 * likely to use. */
	out = -12345.0;
	check("double: accepts a whole number stored as int64",
		viewport_json_double(root, "a_whole", &out));
	check("double: reads the whole number as 1.0", out == 1.0);

	out = -12345.0;
	check("double: rejects a string",
		!viewport_json_double(root, "a_string", &out));
	check("double: rejects a bool",
		!viewport_json_double(root, "a_bool", &out));
	check("double: rejects an object",
		!viewport_json_double(root, "an_object", &out));
	check("double: rejects an array",
		!viewport_json_double(root, "an_array", &out));
	check("double: rejects null",
		!viewport_json_double(root, "a_null", &out));
	check("double: rejects an absent member",
		!viewport_json_double(root, missing, &out));
	check("double: rejects a NULL object",
		!viewport_json_double(NULL, "a_double", &out));
	check("double: leaves the output alone when it refuses", out == -12345.0);
}

static void test_bool(JsonObject *root)
{
	bool out = false;

	check("bool: reads a bool", viewport_json_bool(root, "a_bool", &out));
	check("bool: reads the right bool", out == true);

	out = false;
	/* 1 is not true here. Every other language in the stack would say it is,
	 * which is exactly why the accessor has to say otherwise once. */
	check("bool: rejects an int", !viewport_json_bool(root, "an_int", &out));
	check("bool: rejects a double",
		!viewport_json_bool(root, "a_double", &out));
	check("bool: rejects a string",
		!viewport_json_bool(root, "a_string", &out));
	check("bool: rejects an object",
		!viewport_json_bool(root, "an_object", &out));
	check("bool: rejects an array",
		!viewport_json_bool(root, "an_array", &out));
	check("bool: rejects null", !viewport_json_bool(root, "a_null", &out));
	check("bool: rejects an absent member",
		!viewport_json_bool(root, missing, &out));
	check("bool: rejects a NULL object",
		!viewport_json_bool(NULL, "a_bool", &out));
	check("bool: leaves the output alone when it refuses", out == false);
}

static void test_string(JsonObject *root)
{
	const char *out = viewport_json_string(root, "a_string");
	check("string: reads a string", out != NULL);
	check("string: reads the right string",
		out != NULL && strcmp(out, "hello") == 0);

	/* NULL, not a pointer to nothing: the whole point is that the caller's
	 * strcmp() never runs. */
	check("string: rejects an int",
		viewport_json_string(root, "an_int") == NULL);
	check("string: rejects a double",
		viewport_json_string(root, "a_double") == NULL);
	check("string: rejects a bool",
		viewport_json_string(root, "a_bool") == NULL);
	check("string: rejects an object",
		viewport_json_string(root, "an_object") == NULL);
	check("string: rejects an array",
		viewport_json_string(root, "an_array") == NULL);
	check("string: rejects null",
		viewport_json_string(root, "a_null") == NULL);
	check("string: rejects an absent member",
		viewport_json_string(root, missing) == NULL);
	check("string: rejects a NULL object",
		viewport_json_string(NULL, "a_string") == NULL);
}

static void test_object(JsonObject *root)
{
	JsonObject *inner = viewport_json_object(root, "an_object");
	check("object: reads an object", inner != NULL);

	int value = -12345;
	check("object: the object it returns is usable",
		viewport_json_int(inner, "inner", &value) && value == 3);

	check("object: rejects an int",
		viewport_json_object(root, "an_int") == NULL);
	check("object: rejects a double",
		viewport_json_object(root, "a_double") == NULL);
	check("object: rejects a bool",
		viewport_json_object(root, "a_bool") == NULL);
	check("object: rejects a string",
		viewport_json_object(root, "a_string") == NULL);
	check("object: rejects an array",
		viewport_json_object(root, "an_array") == NULL);
	check("object: rejects null",
		viewport_json_object(root, "a_null") == NULL);
	check("object: rejects an absent member",
		viewport_json_object(root, missing) == NULL);
	check("object: rejects a NULL object",
		viewport_json_object(NULL, "an_object") == NULL);
}

/* The reason every accessor tolerates a NULL object rather than asserting on
 * one: a caller reads a nested object and then reads out of it, and the whole
 * saving is that it does not have to test in between. If that chain crashed on
 * a message whose "rect" was a string, the checks would have moved rather than
 * disappeared. */
static void test_chaining(JsonObject *root)
{
	JsonObject *not_an_object = viewport_json_object(root, "a_string");
	int value = -12345;

	check("chain: reading out of a refused object is safe",
		!viewport_json_int(not_an_object, "anything", &value) &&
			viewport_json_string(not_an_object, "anything") == NULL &&
			viewport_json_object(not_an_object, "anything") == NULL);
	check("chain: leaves the output alone", value == -12345);
}

int main(void)
{
	JsonParser *parser = json_parser_new();
	GError *error = NULL;

	if (!json_parser_load_from_data(parser, document, -1, &error)) {
		fprintf(stderr, "FAIL the test document does not parse: %s\n",
			error->message);
		g_error_free(error);
		g_object_unref(parser);
		return 1;
	}

	JsonObject *root = json_node_get_object(json_parser_get_root(parser));

	test_int(root);
	test_double(root);
	test_bool(root);
	test_string(root);
	test_object(root);
	test_chaining(root);

	g_object_unref(parser);

	printf("%s %d failure(s)\n", failures == 0 ? "ok  " : "FAIL", failures);
	return failures == 0 ? 0 : 1;
}
