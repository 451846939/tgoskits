/* Copyright 2026 The TGOSKits Authors */
/* SPDX-License-Identifier: Apache-2.0 */

#ifndef FREERTOS_IP_CONFIG_H
#define FREERTOS_IP_CONFIG_H

#include "platform.h"

#define ipconfigUSE_IPv4                              1
#define ipconfigUSE_IPv6                              0
#define ipconfigIPv4_BACKWARD_COMPATIBLE              1
#define ipconfigBYTE_ORDER                            pdFREERTOS_LITTLE_ENDIAN
#define ipconfigUSE_TCP                               1
#define ipconfigUSE_UDP                               0
#define ipconfigUSE_DHCP                              0
#define ipconfigUSE_DNS                               0
#define ipconfigUSE_DNS_CACHE                         0
#define ipconfigUSE_LLMNR                             0
#define ipconfigUSE_NBNS                              0
#define ipconfigUSE_MDNS                              0
#define ipconfigUSE_NETWORK_EVENT_HOOK                1
#define ipconfigUSE_NETWORK_EVENT_HOOK_MULTI          0
#define ipconfigNETWORK_MTU                           1500
#define ipconfigNUM_NETWORK_BUFFER_DESCRIPTORS        32
#define ipconfigEVENT_QUEUE_LENGTH                    40
#define ipconfigIP_TASK_PRIORITY                      7
#define ipconfigIP_TASK_STACK_SIZE_WORDS              2048
#define ipconfigARP_CACHE_ENTRIES                     8
#define ipconfigMAX_ARP_RETRANSMISSIONS               5
#define ipconfigMAX_ARP_AGE                           150
#define ipconfigMAX_IP_TASK_SLEEP_TIME                pdMS_TO_TICKS( 1000U )
#define ipconfigETHERNET_DRIVER_FILTERS_FRAME_TYPES   1
#define ipconfigDRIVER_INCLUDED_TX_IP_CHECKSUM        0
#define ipconfigDRIVER_INCLUDED_RX_IP_CHECKSUM        0
#define ipconfigTCP_MSS                               1460
#define ipconfigTCP_TX_BUFFER_LENGTH                  ( 4U * ipconfigTCP_MSS )
#define ipconfigTCP_RX_BUFFER_LENGTH                  ( 4U * ipconfigTCP_MSS )
#define ipconfigUSE_TCP_WIN                           0
#define ipconfigTCP_WIN_SEG_COUNT                     4
#define ipconfigSOCK_DEFAULT_RECEIVE_BLOCK_TIME       pdMS_TO_TICKS( 5000U )
#define ipconfigSOCK_DEFAULT_SEND_BLOCK_TIME          pdMS_TO_TICKS( 5000U )
#define ipconfigINCLUDE_FULL_INET_ADDR                1
#define ipconfigREPLY_TO_INCOMING_PINGS               1
#define ipconfigSUPPORT_OUTGOING_PINGS                0
#define ipconfigSUPPORT_SELECT_FUNCTION               0
#define ipconfigUSE_SIGNALS                           0
#define ipconfigUSE_CALLBACKS                         0
#define ipconfigUSE_LINKED_RX_MESSAGES                0
#define ipconfigZERO_COPY_RX_DRIVER                   0
#define ipconfigZERO_COPY_TX_DRIVER                   0
#define ipconfigHAS_PRINTF                            0
#define ipconfigHAS_DEBUG_PRINTF                      0

#endif
