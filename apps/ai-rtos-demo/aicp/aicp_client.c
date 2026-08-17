// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#include "aicp_client.h"

#include <errno.h>
#include <string.h>

static void emit_event(
    const struct aicp_client_ops *ops,
    enum aicp_client_event_kind kind,
    const struct aicp_header *request,
    const struct aicp_header *response,
    int result) {
    if (ops != NULL && ops->on_event != NULL) {
        const struct aicp_client_event event = {
            .kind = kind,
            .request = request,
            .response = response,
            .result = result,
        };
        ops->on_event(ops->context, &event);
    }
}

static uint64_t monotonic_ns(const struct aicp_client_ops *ops) {
    if (ops == NULL || ops->monotonic_ns == NULL) {
        return 0;
    }
    return ops->monotonic_ns(ops->context);
}

static int send_request(
    struct aicp_stream *stream,
    const struct aicp_header *request,
    const void *payload,
    const struct aicp_client_ops *ops) {
    emit_event(ops, AICP_CLIENT_TX_BEGIN, request, NULL, 0);
    const int result = aicp_stream_send_frame(stream, *request, payload);
    emit_event(ops, AICP_CLIENT_TX_COMPLETE, request, NULL, result);
    return result;
}

int aicp_client_session_send_hello(
    struct aicp_stream *stream,
    uint32_t *next_seq,
    const void *payload,
    uint32_t payload_len,
    const struct aicp_client_ops *ops) {
    if (next_seq == NULL || (payload_len != 0 && payload == NULL) ||
        payload_len > AICP_MAX_PAYLOAD) {
        return -EINVAL;
    }

    const struct aicp_header request = aicp_make_header(
        AICP_MSG_HELLO,
        0,
        payload_len,
        (*next_seq)++,
        monotonic_ns(ops),
        AICP_OK);
    return send_request(stream, &request, payload, ops);
}

int aicp_client_session_transact_control(
    struct aicp_stream *stream,
    uint32_t *next_seq,
    const struct aicp_control_payload *control,
    struct aicp_status_payload *status,
    uint64_t *rtt_ns,
    const struct aicp_client_ops *ops) {
    if (next_seq == NULL || control == NULL || status == NULL) {
        return -EINVAL;
    }

    const uint64_t start = monotonic_ns(ops);
    const struct aicp_header request = aicp_make_header(
        AICP_MSG_CONTROL_SET,
        AICP_FLAG_ACK_REQUIRED,
        sizeof(*control),
        (*next_seq)++,
        start,
        AICP_OK);
    int result = send_request(stream, &request, control, ops);
    if (result != 0) {
        return result;
    }

    uint8_t payload[AICP_MAX_PAYLOAD];
    struct aicp_header response;
    result = aicp_stream_recv_frame(stream, &response, payload, sizeof(payload));
    emit_event(
        ops,
        AICP_CLIENT_RX_COMPLETE,
        &request,
        result == 0 ? &response : NULL,
        result);
    if (result != 0) {
        return result;
    }

    if (response.version != AICP_VERSION) {
        return -EPROTO;
    }
    if (rtt_ns != NULL) {
        *rtt_ns = monotonic_ns(ops) - start;
    }
    if (response.msg_type == AICP_MSG_ERROR) {
        return -EPROTO;
    }
    if (response.msg_type != AICP_MSG_STATUS || response.seq != request.seq ||
        response.payload_len != sizeof(*status)) {
        return -EPROTO;
    }

    memcpy(status, payload, sizeof(*status));
    return 0;
}
