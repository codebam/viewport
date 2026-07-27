/* SPDX-License-Identifier: MIT
 *
 * System statistics for the shell's status bar.
 *
 * This is the one part of a status bar that cannot live in the shell. The page
 * is loaded from file:// or http://, and neither origin can read /proc — so
 * the compositor samples it and pushes the numbers over the same IPC channel
 * everything else uses. Formatting, icons, layout and which modules exist at
 * all remain entirely the shell's business; this only supplies raw values.
 *
 * Everything derivable in JS is deliberately absent. The clock is not here:
 * the page has Date().
 */
#define _DEFAULT_SOURCE
#define _POSIX_C_SOURCE 200809L

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/statvfs.h>

#include <glib.h>
#include <json-glib/json-glib.h>

#include <wlr/util/log.h>

#include "viewport-shell.h"

struct viewport_status {
	struct viewport_server *server;
	guint timer_id;

	/* CPU time is a counter, so a percentage needs two samples. */
	unsigned long long last_total;
	unsigned long long last_idle;

	/* Likewise for network bytes. */
	unsigned long long last_rx;
	unsigned long long last_tx;
	gint64 last_sample_us;
};

/* Fraction of CPU time spent doing work since the previous sample. */
static double sample_cpu(struct viewport_status *status)
{
	FILE *file = fopen("/proc/stat", "r");
	if (file == NULL) {
		return -1;
	}

	unsigned long long user, nice, system, idle, iowait, irq, softirq, steal;
	int fields = fscanf(file, "cpu %llu %llu %llu %llu %llu %llu %llu %llu",
		&user, &nice, &system, &idle, &iowait, &irq, &softirq, &steal);
	fclose(file);

	if (fields < 8) {
		return -1;
	}

	unsigned long long idle_all = idle + iowait;
	unsigned long long total = user + nice + system + idle_all + irq +
		softirq + steal;

	double percent = -1;
	if (status->last_total != 0 && total > status->last_total) {
		double delta_total = (double)(total - status->last_total);
		double delta_idle = (double)(idle_all - status->last_idle);
		percent = 100.0 * (delta_total - delta_idle) / delta_total;
	}

	status->last_total = total;
	status->last_idle = idle_all;
	return percent;
}

/* Used memory as a percentage, matching what `free` calls "used". */
static double sample_memory(void)
{
	FILE *file = fopen("/proc/meminfo", "r");
	if (file == NULL) {
		return -1;
	}

	unsigned long long total = 0, available = 0;
	char line[256];
	while (fgets(line, sizeof(line), file) != NULL) {
		if (sscanf(line, "MemTotal: %llu kB", &total) == 1) {
			continue;
		}
		if (sscanf(line, "MemAvailable: %llu kB", &available) == 1) {
			break;
		}
	}
	fclose(file);

	if (total == 0) {
		return -1;
	}
	return 100.0 * (double)(total - available) / (double)total;
}

/* Combined receive and transmit rate in bytes per second. */
static void sample_network(struct viewport_status *status, double *rx_rate,
	double *tx_rate)
{
	*rx_rate = *tx_rate = 0;

	FILE *file = fopen("/proc/net/dev", "r");
	if (file == NULL) {
		return;
	}

	unsigned long long rx_total = 0, tx_total = 0;
	char line[512];
	/* Two header lines. */
	if (fgets(line, sizeof(line), file) == NULL ||
			fgets(line, sizeof(line), file) == NULL) {
		fclose(file);
		return;
	}

	while (fgets(line, sizeof(line), file) != NULL) {
		char name[64];
		unsigned long long rx, tx;
		if (sscanf(line, " %63[^:]: %llu %*u %*u %*u %*u %*u %*u %*u %llu",
				name, &rx, &tx) != 3) {
			continue;
		}
		/* Loopback traffic is not "network activity" in any useful sense. */
		if (strcmp(name, "lo") == 0) {
			continue;
		}
		rx_total += rx;
		tx_total += tx;
	}
	fclose(file);

	/* Both counters have to be checked, not just the receive one. They are
	 * sums over the interfaces that exist right now, so an interface going
	 * away — a VPN dropping, a dock unplugged — makes the total go backwards.
	 * Unsigned subtraction then wraps, and the bar showed a transmit rate of
	 * some exabytes per second for one sample. */
	gint64 now = g_get_monotonic_time();
	if (status->last_sample_us != 0 && rx_total >= status->last_rx &&
			tx_total >= status->last_tx) {
		double seconds = (double)(now - status->last_sample_us) / 1e6;
		if (seconds > 0) {
			*rx_rate = (double)(rx_total - status->last_rx) / seconds;
			*tx_rate = (double)(tx_total - status->last_tx) / seconds;
		}
	}

	status->last_rx = rx_total;
	status->last_tx = tx_total;
	status->last_sample_us = now;
}

static void sample_disk(double *free_bytes, double *total_bytes)
{
	struct statvfs vfs;
	*free_bytes = *total_bytes = 0;

	if (statvfs("/", &vfs) == 0) {
		*free_bytes = (double)vfs.f_bavail * (double)vfs.f_frsize;
		*total_bytes = (double)vfs.f_blocks * (double)vfs.f_frsize;
	}
}

static gboolean sample_and_publish(gpointer user_data)
{
	struct viewport_status *status = user_data;

	double cpu = sample_cpu(status);
	double memory = sample_memory();
	double rx, tx;
	sample_network(status, &rx, &tx);
	double disk_free, disk_total;
	sample_disk(&disk_free, &disk_total);

	double load[3] = { 0, 0, 0 };
	getloadavg(load, 3);

	JsonBuilder *builder = json_builder_new();
	json_builder_begin_object(builder);
	json_builder_set_member_name(builder, "type");
	json_builder_add_string_value(builder, "status.update");

	json_builder_set_member_name(builder, "cpu");
	json_builder_add_double_value(builder, cpu);
	json_builder_set_member_name(builder, "memory");
	json_builder_add_double_value(builder, memory);
	json_builder_set_member_name(builder, "load");
	json_builder_add_double_value(builder, load[0]);
	json_builder_set_member_name(builder, "net_rx");
	json_builder_add_double_value(builder, rx);
	json_builder_set_member_name(builder, "net_tx");
	json_builder_add_double_value(builder, tx);
	json_builder_set_member_name(builder, "disk_free");
	json_builder_add_double_value(builder, disk_free);
	json_builder_set_member_name(builder, "disk_total");
	json_builder_add_double_value(builder, disk_total);

	json_builder_end_object(builder);

	JsonGenerator *generator = json_generator_new();
	JsonNode *root = json_builder_get_root(builder);
	json_generator_set_root(generator, root);
	char *text = json_generator_to_data(generator, NULL);

	viewport_ipc_broadcast(status->server, text);

	g_free(text);
	json_node_free(root);
	g_object_unref(generator);
	g_object_unref(builder);

	return G_SOURCE_CONTINUE;
}

struct viewport_status *viewport_status_create(struct viewport_server *server)
{
	struct viewport_status *status = calloc(1, sizeof(*status));
	if (status == NULL) {
		return NULL;
	}
	status->server = server;

	/* Prime the counters so the first published sample is a real rate rather
	 * than a spike measured against zero. */
	sample_cpu(status);
	double rx, tx;
	sample_network(status, &rx, &tx);

	/* Two seconds matches the shortest interval in a typical waybar config;
	 * anything faster costs more than it tells you. */
	status->timer_id = g_timeout_add_seconds(2, sample_and_publish, status);
	return status;
}

void viewport_status_destroy(struct viewport_status *status)
{
	if (status == NULL) {
		return;
	}
	if (status->timer_id != 0) {
		g_source_remove(status->timer_id);
	}
	free(status);
}
