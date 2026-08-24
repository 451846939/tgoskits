// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#include <errno.h>
#include <stdio.h>
#include <unistd.h>

#ifndef AICP_YOLO_SERVER
#define AICP_YOLO_SERVER "10.0.3.2"
#endif

#ifndef AICP_YOLO_CLIENT
#define AICP_YOLO_CLIENT "10.0.3.3"
#endif

#ifndef AICP_YOLO_NET_PREFIX
#define AICP_YOLO_NET_PREFIX "10.0.3.0"
#endif

int main(void)
{
    char *const argv[] = {
        "/bin/aicp_yolov8_rust_onnx",
        "--aicp-host",
        AICP_YOLO_SERVER,
        "--aicp-port",
        "8800",
        "--client-ip",
        AICP_YOLO_CLIENT,
        "--net-prefix",
        AICP_YOLO_NET_PREFIX,
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

    printf("AICP_YOLO_LINUX_LAUNCH host=%s port=8800 client=%s\n",
           AICP_YOLO_SERVER, AICP_YOLO_CLIENT);
    execv(argv[0], argv);
    printf("AICP_YOLO_RTTHREAD_FATAL stage=execv errno=%d\n", errno);
    for (;;) {
        pause();
    }
}
