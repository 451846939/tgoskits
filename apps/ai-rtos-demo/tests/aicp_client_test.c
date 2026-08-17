// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#include "aicp_client.h"
#include "aicp_posix_stream.h"

#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

struct server_case {
    int socket;
    int corrupt_response_seq;
    int unsupported_response_version;
    int result;
};

struct client_context {
    uint64_t now;
    unsigned tx_begin;
    unsigned tx_complete;
    unsigned rx_complete;
};

static uint64_t fake_monotonic_ns(void *context) {
    struct client_context *client = context;
    client->now += 1000;
    return client->now;
}

static void record_event(
    void *context,
    const struct aicp_client_event *event) {
    struct client_context *trace = context;
    switch (event->kind) {
    case AICP_CLIENT_TX_BEGIN:
        trace->tx_begin++;
        break;
    case AICP_CLIENT_TX_COMPLETE:
        trace->tx_complete++;
        break;
    case AICP_CLIENT_RX_COMPLETE:
        trace->rx_complete++;
        break;
    }
}

static int send_status_response(
    struct aicp_stream *stream,
    struct aicp_header header,
    const struct aicp_status_payload *status) {
    uint8_t wire[AICP_HEADER_LEN];

    header.magic = AICP_MAGIC;
    header.header_len = AICP_HEADER_LEN;
    header.crc16 = aicp_frame_crc(header, status);
    aicp_header_encode(&header, wire);
    const int result = aicp_stream_write_full(stream, wire, sizeof(wire));
    if (result != 0) {
        return result;
    }
    return aicp_stream_write_full(stream, status, sizeof(*status));
}

static void *serve_client(void *argument) {
    struct server_case *test = argument;
    struct aicp_posix_stream stream;
    uint8_t payload[AICP_MAX_PAYLOAD];
    struct aicp_header request;

    aicp_posix_stream_init(&stream, test->socket);

    test->result = aicp_stream_recv_frame(
        &stream.stream, &request, payload, sizeof(payload));
    if (test->result != 0 || request.msg_type != AICP_MSG_HELLO) {
        test->result = -1;
        return NULL;
    }

    test->result = aicp_stream_recv_frame(
        &stream.stream, &request, payload, sizeof(payload));
    if (test->result != 0 || request.msg_type != AICP_MSG_CONTROL_SET ||
        request.payload_len != sizeof(struct aicp_control_payload)) {
        test->result = -1;
        return NULL;
    }

    const struct aicp_status_payload status = {
        .setpoint = 0.25f,
        .measured = 0.5f,
        .control_output = 0.75f,
        .error = -0.25f,
        .mode = 1,
        .applied_seq = request.seq,
    };
    const uint32_t response_seq =
        test->corrupt_response_seq ? request.seq + 1u : request.seq;
    const struct aicp_header response = aicp_make_header(
        AICP_MSG_STATUS,
        0,
        sizeof(status),
        response_seq,
        1234,
        AICP_OK);
    struct aicp_header wire_response = response;
    if (test->unsupported_response_version) {
        wire_response.version = AICP_VERSION + 1u;
    }
    test->result = send_status_response(&stream.stream, wire_response, &status);
    return NULL;
}

static int run_case(int corrupt_response_seq, int unsupported_response_version) {
    int sockets[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) != 0) {
        return -1;
    }

    struct server_case server = {
        .socket = sockets[1],
        .corrupt_response_seq = corrupt_response_seq,
        .unsupported_response_version = unsupported_response_version,
        .result = 0,
    };
    pthread_t thread;
    if (pthread_create(&thread, NULL, serve_client, &server) != 0) {
        close(sockets[0]);
        close(sockets[1]);
        return -1;
    }

    struct client_context trace = {0};
    const struct aicp_client_ops ops = {
        .monotonic_ns = fake_monotonic_ns,
        .on_event = record_event,
        .context = &trace,
    };
    uint32_t seq = 1;
    const char hello[] = "{\"role\":\"client-test\"}";
    struct aicp_posix_stream stream;
    aicp_posix_stream_init(&stream, sockets[0]);
    int result = aicp_client_session_send_hello(
        &stream.stream, &seq, hello, sizeof(hello), &ops);

    const struct aicp_control_payload control = {
        .target = 0.25f,
        .kp = 0.5f,
        .ki = 0.1f,
        .kd = 0.01f,
        .feed_forward = 0.2f,
        .mode = 1,
    };
    struct aicp_status_payload status = {
        .setpoint = -1.0f,
        .measured = -1.0f,
        .control_output = -1.0f,
        .error = -1.0f,
        .mode = UINT32_MAX,
        .applied_seq = UINT32_MAX,
    };
    uint64_t rtt_ns = 0;
    if (result == 0) {
        result = aicp_client_session_transact_control(
            &stream.stream, &seq, &control, &status, &rtt_ns, &ops);
    }

    close(sockets[0]);
    pthread_join(thread, NULL);
    close(sockets[1]);

    const int expected = (corrupt_response_seq || unsupported_response_version) ? -EPROTO : 0;
    if (result != expected || server.result != 0 || seq != 3 ||
        trace.tx_begin != 2 || trace.tx_complete != 2 ||
        trace.rx_complete != 1) {
        return -1;
    }
    if (!corrupt_response_seq && !unsupported_response_version &&
        (status.applied_seq != 2 || rtt_ns != 1000)) {
        return -1;
    }
    if (unsupported_response_version &&
        (status.setpoint != -1.0f || status.measured != -1.0f ||
         status.control_output != -1.0f || status.error != -1.0f ||
         status.mode != UINT32_MAX || status.applied_seq != UINT32_MAX)) {
        return -1;
    }
    return 0;
}

int main(void) {
    unsigned passed = 0;
    unsigned failed = 0;

    if (run_case(0, 0) == 0) {
        passed++;
    } else {
        failed++;
    }
    if (run_case(1, 0) == 0) {
        passed++;
    } else {
        failed++;
    }
    if (run_case(0, 1) == 0) {
        passed++;
    } else {
        failed++;
    }

    printf("AICP_CLIENT_SUMMARY passed=%u failed=%u\n", passed, failed);
    return failed == 0 ? 0 : 1;
}
