// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#include <errno.h>
#include <stdio.h>
#include <unistd.h>

int main(void)
{
    char *const argv[] = {
        "/bin/aicp_yolov8_rust_onnx",
        "--aicp-host",
        "10.0.2.15",
        "--aicp-port",
        "8800",
        "--client-ip",
        "10.0.2.14",
        "--net-prefix",
        "10.0.2.0",
        "--netmask",
        "255.255.255.0",
        "--iface",
        "eth0",
        "--server-mac",
        "52:54:00:aa:03:02",
        "--connect-timeout-ms",
        "1000",
        "--connect-retries",
        "180",
        "--connect-retry-delay-ms",
        "1000",
        NULL,
    };

    printf("AICP_YOLO_RTTHREAD_LAUNCH host=10.0.2.15 port=8800 client=10.0.2.14\n");
    execv(argv[0], argv);
    printf("AICP_YOLO_RTTHREAD_FATAL stage=execv errno=%d\n", errno);
    for (;;) {
        pause();
    }
}
