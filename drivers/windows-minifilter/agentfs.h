#pragma once

#define AGENTFS_PORT_NAME L"\\AgentFsPort"
#define AGENTFS_MAX_CHARS 1024
#define AGENTFS_ENV_ID_CHARS 128
#define AGENTFS_REPLY_CHARS 512
#define AGENTFS_IOCTL_VERSION 1

typedef enum _AGENTFS_REQUEST_KIND {
    AgentFsRegisterProcess = 1,
    AgentFsUnregisterProcess = 2,
    AgentFsCheck = 3,
} AGENTFS_REQUEST_KIND;

typedef struct _AGENTFS_REQUEST {
    ULONG Version;
    ULONG Kind;
    ULONG ProcessId;
    ULONG Reserved;
    WCHAR EnvId[AGENTFS_ENV_ID_CHARS];
    WCHAR SourceRoot[AGENTFS_MAX_CHARS];
    WCHAR LowerRoot[AGENTFS_MAX_CHARS];
    WCHAR UpperRoot[AGENTFS_MAX_CHARS];
    WCHAR WhiteoutRoot[AGENTFS_MAX_CHARS];
} AGENTFS_REQUEST, *PAGENTFS_REQUEST;

typedef struct _AGENTFS_REPLY {
    ULONG Status;
    ULONG Win32Error;
    WCHAR Message[AGENTFS_REPLY_CHARS];
} AGENTFS_REPLY, *PAGENTFS_REPLY;
