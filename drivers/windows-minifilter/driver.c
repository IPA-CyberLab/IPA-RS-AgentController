#include <fltKernel.h>
#include <dontuse.h>
#include "agentfs.h"

#define AGENTFS_TAG 'sfga'
#define AGENTFS_REPLY_OK 0
#define AGENTFS_REPLY_ERROR 1

typedef struct _AGENTFS_ENV {
    LIST_ENTRY Link;
    HANDLE ProcessId;
    UNICODE_STRING EnvId;
    UNICODE_STRING SourceRoot;
    UNICODE_STRING LowerRoot;
    UNICODE_STRING UpperRoot;
    UNICODE_STRING WhiteoutRoot;
} AGENTFS_ENV, *PAGENTFS_ENV;

typedef struct _AGENTFS_DIR_CONTEXT {
    UNICODE_STRING VisiblePath;
    UNICODE_STRING LowerPath;
    UNICODE_STRING UpperPath;
    UNICODE_STRING WhiteoutPath;
    BOOLEAN EnumeratingUpperPath;
} AGENTFS_DIR_CONTEXT, *PAGENTFS_DIR_CONTEXT;

typedef struct _AGENTFS_DIR_STATE {
    LIST_ENTRY Link;
    PFILE_OBJECT FileObject;
    BOOLEAN UpperMerged;
} AGENTFS_DIR_STATE, *PAGENTFS_DIR_STATE;

static PFLT_FILTER gFilter;
static PFLT_PORT gServerPort;
static PFLT_PORT gClientPort;
static FAST_MUTEX gEnvLock;
static LIST_ENTRY gEnvs;
static FAST_MUTEX gDirStateLock;
static LIST_ENTRY gDirStates;

DRIVER_INITIALIZE DriverEntry;

static NTSTATUS AgentFsUnload(_In_ FLT_FILTER_UNLOAD_FLAGS Flags);
static FLT_PREOP_CALLBACK_STATUS AgentFsPreCreate(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _Flt_CompletionContext_Outptr_ PVOID *CompletionContext);
static FLT_PREOP_CALLBACK_STATUS AgentFsPreSetInformation(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _Flt_CompletionContext_Outptr_ PVOID *CompletionContext);
static FLT_PREOP_CALLBACK_STATUS AgentFsPreDirectoryControl(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _Flt_CompletionContext_Outptr_ PVOID *CompletionContext);
static FLT_PREOP_CALLBACK_STATUS AgentFsPreFileSystemControl(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _Flt_CompletionContext_Outptr_ PVOID *CompletionContext);
static FLT_POSTOP_CALLBACK_STATUS AgentFsPostDirectoryControl(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _In_opt_ PVOID CompletionContext,
    _In_ FLT_POST_OPERATION_FLAGS Flags);
static FLT_PREOP_CALLBACK_STATUS AgentFsPreCleanup(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _Flt_CompletionContext_Outptr_ PVOID *CompletionContext);

static NTSTATUS AgentFsConnect(
    _In_ PFLT_PORT ClientPort,
    _In_opt_ PVOID ServerPortCookie,
    _In_reads_bytes_opt_(SizeOfContext) PVOID ConnectionContext,
    _In_ ULONG SizeOfContext,
    _Outptr_result_maybenull_ PVOID *ConnectionCookie);
static VOID AgentFsDisconnect(_In_opt_ PVOID ConnectionCookie);
static NTSTATUS AgentFsMessage(
    _In_opt_ PVOID PortCookie,
    _In_reads_bytes_opt_(InputBufferLength) PVOID InputBuffer,
    _In_ ULONG InputBufferLength,
    _Out_writes_bytes_to_opt_(OutputBufferLength, *ReturnOutputBufferLength) PVOID OutputBuffer,
    _In_ ULONG OutputBufferLength,
    _Out_ PULONG ReturnOutputBufferLength);
static VOID AgentFsProcessNotify(
    _Inout_ PEPROCESS Process,
    _In_ HANDLE ProcessId,
    _Inout_opt_ PPS_CREATE_NOTIFY_INFO CreateInfo);
static BOOLEAN AgentFsPathExists(_In_ PFLT_INSTANCE Instance, _In_ PCUNICODE_STRING Path);
static BOOLEAN AgentFsPathIsDirectory(_In_ PFLT_INSTANCE Instance, _In_ PCUNICODE_STRING Path);
static VOID AgentFsResetDirState(_In_ PFILE_OBJECT FileObject);
static BOOLEAN AgentFsDirUpperAlreadyMerged(_In_ PFILE_OBJECT FileObject);
static VOID AgentFsMarkDirUpperMerged(_In_ PFILE_OBJECT FileObject);
static VOID AgentFsRemoveDirState(_In_opt_ PFILE_OBJECT FileObject);

CONST FLT_OPERATION_REGISTRATION Callbacks[] = {
    { IRP_MJ_CREATE, 0, AgentFsPreCreate, NULL },
    { IRP_MJ_SET_INFORMATION, 0, AgentFsPreSetInformation, NULL },
    { IRP_MJ_DIRECTORY_CONTROL, 0, AgentFsPreDirectoryControl, AgentFsPostDirectoryControl },
    { IRP_MJ_FILE_SYSTEM_CONTROL, 0, AgentFsPreFileSystemControl, NULL },
    { IRP_MJ_CLEANUP, 0, AgentFsPreCleanup, NULL },
    { IRP_MJ_OPERATION_END }
};

CONST FLT_REGISTRATION FilterRegistration = {
    sizeof(FLT_REGISTRATION),
    FLT_REGISTRATION_VERSION,
    0,
    NULL,
    Callbacks,
    AgentFsUnload,
    NULL,
    NULL,
    NULL,
    NULL,
    NULL,
    NULL,
    NULL,
    NULL,
    NULL
};

static VOID AgentFsFreeUnicode(_Inout_ PUNICODE_STRING String)
{
    if (String->Buffer != NULL) {
        ExFreePoolWithTag(String->Buffer, AGENTFS_TAG);
    }
    RtlZeroMemory(String, sizeof(*String));
}

static VOID AgentFsFreeEnv(_In_ PAGENTFS_ENV Env)
{
    AgentFsFreeUnicode(&Env->EnvId);
    AgentFsFreeUnicode(&Env->SourceRoot);
    AgentFsFreeUnicode(&Env->LowerRoot);
    AgentFsFreeUnicode(&Env->UpperRoot);
    AgentFsFreeUnicode(&Env->WhiteoutRoot);
    ExFreePoolWithTag(Env, AGENTFS_TAG);
}

static VOID AgentFsFreeDirContext(_In_opt_ PAGENTFS_DIR_CONTEXT Context)
{
    if (Context == NULL) {
        return;
    }
    AgentFsFreeUnicode(&Context->VisiblePath);
    AgentFsFreeUnicode(&Context->LowerPath);
    AgentFsFreeUnicode(&Context->UpperPath);
    AgentFsFreeUnicode(&Context->WhiteoutPath);
    ExFreePoolWithTag(Context, AGENTFS_TAG);
}

static NTSTATUS AgentFsDupUserString(
    _Out_ PUNICODE_STRING Destination,
    _In_reads_(MaxChars) const WCHAR *Source,
    _In_ USHORT MaxChars)
{
    USHORT chars = 0;
    RtlZeroMemory(Destination, sizeof(*Destination));
    while (chars < MaxChars && Source[chars] != L'\0') {
        chars++;
    }
    if (chars == 0 || chars == MaxChars) {
        return STATUS_INVALID_PARAMETER;
    }
    Destination->Length = chars * sizeof(WCHAR);
    Destination->MaximumLength = Destination->Length + sizeof(WCHAR);
    Destination->Buffer = ExAllocatePool2(POOL_FLAG_NON_PAGED, Destination->MaximumLength, AGENTFS_TAG);
    if (Destination->Buffer == NULL) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    RtlCopyMemory(Destination->Buffer, Source, Destination->Length);
    Destination->Buffer[chars] = L'\0';
    return STATUS_SUCCESS;
}

static BOOLEAN AgentFsStartsWithPath(_In_ PCUNICODE_STRING Path, _In_ PCUNICODE_STRING Root)
{
    if (Path->Length < Root->Length) {
        return FALSE;
    }
    UNICODE_STRING prefix;
    prefix.Buffer = Path->Buffer;
    prefix.Length = Root->Length;
    prefix.MaximumLength = Root->Length;
    if (!RtlEqualUnicodeString(&prefix, Root, TRUE)) {
        return FALSE;
    }
    if (Path->Length == Root->Length) {
        return TRUE;
    }
    WCHAR next = Path->Buffer[Root->Length / sizeof(WCHAR)];
    return next == L'\\' || next == L'/';
}

static NTSTATUS AgentFsJoinRedirectPath(
    _Out_ PUNICODE_STRING Redirect,
    _In_ PCUNICODE_STRING Root,
    _In_ PCUNICODE_STRING SourceRoot,
    _In_ PCUNICODE_STRING OriginalPath)
{
    USHORT suffixBytes = OriginalPath->Length - SourceRoot->Length;
    USHORT totalBytes = Root->Length + suffixBytes;
    Redirect->Length = totalBytes;
    Redirect->MaximumLength = totalBytes + sizeof(WCHAR);
    Redirect->Buffer = ExAllocatePool2(POOL_FLAG_NON_PAGED, Redirect->MaximumLength, AGENTFS_TAG);
    if (Redirect->Buffer == NULL) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    RtlCopyMemory(Redirect->Buffer, Root->Buffer, Root->Length);
    RtlCopyMemory(
        (PUCHAR)Redirect->Buffer + Root->Length,
        (PUCHAR)OriginalPath->Buffer + SourceRoot->Length,
        suffixBytes);
    Redirect->Buffer[totalBytes / sizeof(WCHAR)] = L'\0';
    return STATUS_SUCCESS;
}

static NTSTATUS AgentFsStorageToVisiblePath(
    _Out_ PUNICODE_STRING Visible,
    _In_ PCUNICODE_STRING SourceRoot,
    _In_ PCUNICODE_STRING StorageRoot,
    _In_ PCUNICODE_STRING StoragePath)
{
    if (!AgentFsStartsWithPath(StoragePath, StorageRoot)) {
        return STATUS_NOT_FOUND;
    }
    USHORT suffixBytes = StoragePath->Length - StorageRoot->Length;
    USHORT totalBytes = SourceRoot->Length + suffixBytes;
    Visible->Length = totalBytes;
    Visible->MaximumLength = totalBytes + sizeof(WCHAR);
    Visible->Buffer = ExAllocatePool2(POOL_FLAG_NON_PAGED, Visible->MaximumLength, AGENTFS_TAG);
    if (Visible->Buffer == NULL) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    RtlCopyMemory(Visible->Buffer, SourceRoot->Buffer, SourceRoot->Length);
    RtlCopyMemory(
        (PUCHAR)Visible->Buffer + SourceRoot->Length,
        (PUCHAR)StoragePath->Buffer + StorageRoot->Length,
        suffixBytes);
    Visible->Buffer[totalBytes / sizeof(WCHAR)] = L'\0';
    return STATUS_SUCCESS;
}

static NTSTATUS AgentFsEnsureDirectoryTree(_In_ PFLT_INSTANCE Instance, _In_ PCUNICODE_STRING Directory)
{
    if (AgentFsPathExists(Instance, Directory)) {
        return STATUS_SUCCESS;
    }

    USHORT chars = Directory->Length / sizeof(WCHAR);
    USHORT lastSlash = 0;
    for (USHORT i = 0; i < chars; i++) {
        if (Directory->Buffer[i] == L'\\' || Directory->Buffer[i] == L'/') {
            lastSlash = i;
        }
    }
    if (lastSlash > 0) {
        UNICODE_STRING parent;
        parent.Buffer = Directory->Buffer;
        parent.Length = lastSlash * sizeof(WCHAR);
        parent.MaximumLength = parent.Length;
        NTSTATUS parentStatus = AgentFsEnsureDirectoryTree(Instance, &parent);
        if (!NT_SUCCESS(parentStatus)) {
            return parentStatus;
        }
    }

    OBJECT_ATTRIBUTES oa;
    IO_STATUS_BLOCK iosb;
    HANDLE handle = NULL;
    InitializeObjectAttributes(&oa, (PUNICODE_STRING)Directory, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    NTSTATUS status = FltCreateFile(
        gFilter,
        Instance,
        &handle,
        FILE_LIST_DIRECTORY | SYNCHRONIZE,
        &oa,
        &iosb,
        NULL,
        FILE_ATTRIBUTE_DIRECTORY,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN_IF,
        FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
        NULL,
        0,
        0);
    if (NT_SUCCESS(status)) {
        FltClose(handle);
    }
    return status;
}

static NTSTATUS AgentFsEnsureParentDirectory(_In_ PFLT_INSTANCE Instance, _In_ PCUNICODE_STRING Path)
{
    USHORT chars = Path->Length / sizeof(WCHAR);
    USHORT lastSlash = 0;
    for (USHORT i = 0; i < chars; i++) {
        if (Path->Buffer[i] == L'\\' || Path->Buffer[i] == L'/') {
            lastSlash = i;
        }
    }
    if (lastSlash == 0) {
        return STATUS_SUCCESS;
    }
    UNICODE_STRING parent;
    parent.Buffer = Path->Buffer;
    parent.Length = lastSlash * sizeof(WCHAR);
    parent.MaximumLength = parent.Length;
    return AgentFsEnsureDirectoryTree(Instance, &parent);
}

static NTSTATUS AgentFsCreateEmptyFile(_In_ PFLT_INSTANCE Instance, _In_ PCUNICODE_STRING Path)
{
    NTSTATUS status = AgentFsEnsureParentDirectory(Instance, Path);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    OBJECT_ATTRIBUTES oa;
    IO_STATUS_BLOCK iosb;
    HANDLE handle = NULL;
    InitializeObjectAttributes(&oa, (PUNICODE_STRING)Path, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    status = FltCreateFile(
        gFilter,
        Instance,
        &handle,
        FILE_WRITE_DATA | SYNCHRONIZE,
        &oa,
        &iosb,
        NULL,
        FILE_ATTRIBUTE_NORMAL,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN_IF,
        FILE_SYNCHRONOUS_IO_NONALERT,
        NULL,
        0,
        0);
    if (NT_SUCCESS(status)) {
        FltClose(handle);
    }
    return status;
}

static NTSTATUS AgentFsDeletePath(_In_ PCUNICODE_STRING Path)
{
    OBJECT_ATTRIBUTES oa;
    InitializeObjectAttributes(&oa, (PUNICODE_STRING)Path, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    NTSTATUS status = ZwDeleteFile(&oa);
    if (status == STATUS_OBJECT_NAME_NOT_FOUND || status == STATUS_OBJECT_PATH_NOT_FOUND) {
        return STATUS_SUCCESS;
    }
    return status;
}

static NTSTATUS AgentFsSiblingVisiblePath(
    _Out_ PUNICODE_STRING Target,
    _In_ PCUNICODE_STRING SourceVisible,
    _In_reads_bytes_(FileNameLength) PWCH FileName,
    _In_ ULONG FileNameLength)
{
    if (FileNameLength == 0 || FileName == NULL) {
        return STATUS_INVALID_PARAMETER;
    }
    if (FileName[0] == L'\\') {
        Target->Length = (USHORT)FileNameLength;
        Target->MaximumLength = Target->Length + sizeof(WCHAR);
        Target->Buffer = ExAllocatePool2(POOL_FLAG_NON_PAGED, Target->MaximumLength, AGENTFS_TAG);
        if (Target->Buffer == NULL) {
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        RtlCopyMemory(Target->Buffer, FileName, FileNameLength);
        Target->Buffer[Target->Length / sizeof(WCHAR)] = L'\0';
        return STATUS_SUCCESS;
    }

    USHORT chars = SourceVisible->Length / sizeof(WCHAR);
    USHORT lastSlash = 0;
    for (USHORT i = 0; i < chars; i++) {
        if (SourceVisible->Buffer[i] == L'\\' || SourceVisible->Buffer[i] == L'/') {
            lastSlash = i;
        }
    }
    USHORT parentBytes = lastSlash * sizeof(WCHAR);
    USHORT separatorBytes = sizeof(WCHAR);
    USHORT totalBytes = parentBytes + separatorBytes + (USHORT)FileNameLength;
    Target->Length = totalBytes;
    Target->MaximumLength = totalBytes + sizeof(WCHAR);
    Target->Buffer = ExAllocatePool2(POOL_FLAG_NON_PAGED, Target->MaximumLength, AGENTFS_TAG);
    if (Target->Buffer == NULL) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    RtlCopyMemory(Target->Buffer, SourceVisible->Buffer, parentBytes);
    Target->Buffer[parentBytes / sizeof(WCHAR)] = L'\\';
    RtlCopyMemory((PUCHAR)Target->Buffer + parentBytes + separatorBytes, FileName, FileNameLength);
    Target->Buffer[totalBytes / sizeof(WCHAR)] = L'\0';
    return STATUS_SUCCESS;
}

static NTSTATUS AgentFsJoinChildPath(
    _Out_ PUNICODE_STRING Target,
    _In_ PCUNICODE_STRING Root,
    _In_reads_bytes_(NameLength) PCWCH Name,
    _In_ ULONG NameLength)
{
    USHORT separatorBytes = sizeof(WCHAR);
    USHORT totalBytes = Root->Length + separatorBytes + (USHORT)NameLength;
    Target->Length = totalBytes;
    Target->MaximumLength = totalBytes + sizeof(WCHAR);
    Target->Buffer = ExAllocatePool2(POOL_FLAG_NON_PAGED, Target->MaximumLength, AGENTFS_TAG);
    if (Target->Buffer == NULL) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    RtlCopyMemory(Target->Buffer, Root->Buffer, Root->Length);
    Target->Buffer[Root->Length / sizeof(WCHAR)] = L'\\';
    RtlCopyMemory((PUCHAR)Target->Buffer + Root->Length + separatorBytes, Name, NameLength);
    Target->Buffer[totalBytes / sizeof(WCHAR)] = L'\0';
    return STATUS_SUCCESS;
}

static NTSTATUS AgentFsCopyFile(_In_ PFLT_INSTANCE Instance, _In_ PCUNICODE_STRING Source, _In_ PCUNICODE_STRING Target)
{
    if (AgentFsPathExists(Instance, Target)) {
        return STATUS_SUCCESS;
    }
    NTSTATUS status = AgentFsEnsureParentDirectory(Instance, Target);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    OBJECT_ATTRIBUTES sourceOa;
    OBJECT_ATTRIBUTES targetOa;
    IO_STATUS_BLOCK iosb;
    HANDLE sourceHandle = NULL;
    HANDLE targetHandle = NULL;
    InitializeObjectAttributes(&sourceOa, (PUNICODE_STRING)Source, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    InitializeObjectAttributes(&targetOa, (PUNICODE_STRING)Target, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    status = FltCreateFile(
        gFilter,
        Instance,
        &sourceHandle,
        FILE_READ_DATA | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE,
        &sourceOa,
        &iosb,
        NULL,
        FILE_ATTRIBUTE_NORMAL,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN,
        FILE_SYNCHRONOUS_IO_NONALERT,
        NULL,
        0,
        0);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    PSECURITY_DESCRIPTOR securityDescriptor = NULL;
    ULONG securityDescriptorLength = 0;
    SECURITY_INFORMATION securityInformation =
        OWNER_SECURITY_INFORMATION |
        GROUP_SECURITY_INFORMATION |
        DACL_SECURITY_INFORMATION;
    status = ZwQuerySecurityObject(
        sourceHandle,
        securityInformation,
        NULL,
        0,
        &securityDescriptorLength);
    if ((status == STATUS_BUFFER_TOO_SMALL || status == STATUS_BUFFER_OVERFLOW) &&
        securityDescriptorLength != 0) {
        securityDescriptor = ExAllocatePool2(POOL_FLAG_NON_PAGED, securityDescriptorLength, AGENTFS_TAG);
        if (securityDescriptor != NULL) {
            status = ZwQuerySecurityObject(
                sourceHandle,
                securityInformation,
                securityDescriptor,
                securityDescriptorLength,
                &securityDescriptorLength);
            if (!NT_SUCCESS(status)) {
                ExFreePoolWithTag(securityDescriptor, AGENTFS_TAG);
                securityDescriptor = NULL;
            }
        }
    }

    status = FltCreateFile(
        gFilter,
        Instance,
        &targetHandle,
        FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES | WRITE_DAC | WRITE_OWNER | SYNCHRONIZE,
        &targetOa,
        &iosb,
        NULL,
        FILE_ATTRIBUTE_NORMAL,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_CREATE,
        FILE_SYNCHRONOUS_IO_NONALERT,
        NULL,
        0,
        0);
    if (!NT_SUCCESS(status)) {
        if (securityDescriptor != NULL) {
            ExFreePoolWithTag(securityDescriptor, AGENTFS_TAG);
        }
        FltClose(sourceHandle);
        return status;
    }

    PVOID buffer = ExAllocatePool2(POOL_FLAG_NON_PAGED, 64 * 1024, AGENTFS_TAG);
    if (buffer == NULL) {
        FltClose(targetHandle);
        FltClose(sourceHandle);
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    LARGE_INTEGER offset;
    offset.QuadPart = 0;
    for (;;) {
        IO_STATUS_BLOCK readStatus;
        IO_STATUS_BLOCK writeStatus;
        RtlZeroMemory(&readStatus, sizeof(readStatus));
        RtlZeroMemory(&writeStatus, sizeof(writeStatus));
        status = ZwReadFile(
            sourceHandle,
            NULL,
            NULL,
            NULL,
            &readStatus,
            buffer,
            64 * 1024,
            &offset,
            NULL);
        ULONG readBytes = (ULONG)readStatus.Information;
        if (!NT_SUCCESS(status) || readBytes == 0) {
            break;
        }
        status = ZwWriteFile(
            targetHandle,
            NULL,
            NULL,
            NULL,
            &writeStatus,
            buffer,
            readBytes,
            &offset,
            NULL);
        ULONG written = (ULONG)writeStatus.Information;
        if (!NT_SUCCESS(status)) {
            break;
        }
        offset.QuadPart += written;
        if (written != readBytes) {
            status = STATUS_DISK_FULL;
            break;
        }
    }
    if (status == STATUS_END_OF_FILE) {
        status = STATUS_SUCCESS;
    }
    if (NT_SUCCESS(status)) {
        FILE_BASIC_INFORMATION basicInfo;
        RtlZeroMemory(&basicInfo, sizeof(basicInfo));
        status = ZwQueryInformationFile(
            sourceHandle,
            &iosb,
            &basicInfo,
            sizeof(basicInfo),
            FileBasicInformation);
        if (NT_SUCCESS(status)) {
            status = ZwSetInformationFile(
                targetHandle,
                &iosb,
                &basicInfo,
                sizeof(basicInfo),
                FileBasicInformation);
        }
    }
    if (NT_SUCCESS(status) && securityDescriptor != NULL) {
        NTSTATUS securityStatus = ZwSetSecurityObject(
            targetHandle,
            securityInformation,
            securityDescriptor);
        UNREFERENCED_PARAMETER(securityStatus);
    }
    if (securityDescriptor != NULL) {
        ExFreePoolWithTag(securityDescriptor, AGENTFS_TAG);
    }
    ExFreePoolWithTag(buffer, AGENTFS_TAG);
    FltClose(targetHandle);
    FltClose(sourceHandle);
    return status;
}

static NTSTATUS AgentFsRenamePath(
    _In_ PFLT_INSTANCE Instance,
    _In_ PCUNICODE_STRING Source,
    _In_ PCUNICODE_STRING Target,
    _In_ BOOLEAN ReplaceIfExists)
{
    NTSTATUS status = AgentFsEnsureParentDirectory(Instance, Target);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    OBJECT_ATTRIBUTES oa;
    IO_STATUS_BLOCK iosb;
    HANDLE handle = NULL;
    InitializeObjectAttributes(&oa, (PUNICODE_STRING)Source, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    status = FltCreateFile(
        gFilter,
        Instance,
        &handle,
        DELETE | SYNCHRONIZE,
        &oa,
        &iosb,
        NULL,
        FILE_ATTRIBUTE_NORMAL,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        NULL,
        0,
        0);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    ULONG infoBytes = FIELD_OFFSET(FILE_RENAME_INFORMATION, FileName) + Target->Length;
    PFILE_RENAME_INFORMATION info = ExAllocatePool2(POOL_FLAG_NON_PAGED, infoBytes, AGENTFS_TAG);
    if (info == NULL) {
        FltClose(handle);
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    RtlZeroMemory(info, infoBytes);
    info->ReplaceIfExists = ReplaceIfExists;
    info->RootDirectory = NULL;
    info->FileNameLength = Target->Length;
    RtlCopyMemory(info->FileName, Target->Buffer, Target->Length);

    status = ZwSetInformationFile(handle, &iosb, info, infoBytes, FileRenameInformation);
    ExFreePoolWithTag(info, AGENTFS_TAG);
    FltClose(handle);
    return status;
}

static BOOLEAN AgentFsIsDotDirectoryName(_In_reads_bytes_(NameLength) PCWCH Name, _In_ ULONG NameLength)
{
    if (NameLength == sizeof(WCHAR) && Name[0] == L'.') {
        return TRUE;
    }
    return NameLength == 2 * sizeof(WCHAR) && Name[0] == L'.' && Name[1] == L'.';
}

static NTSTATUS AgentFsCopyDirectoryTree(
    _In_ PFLT_INSTANCE Instance,
    _In_ PCUNICODE_STRING Source,
    _In_ PCUNICODE_STRING Target)
{
    NTSTATUS status = AgentFsEnsureDirectoryTree(Instance, Target);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    OBJECT_ATTRIBUTES oa;
    IO_STATUS_BLOCK iosb;
    HANDLE handle = NULL;
    InitializeObjectAttributes(&oa, (PUNICODE_STRING)Source, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    status = FltCreateFile(
        gFilter,
        Instance,
        &handle,
        FILE_LIST_DIRECTORY | SYNCHRONIZE,
        &oa,
        &iosb,
        NULL,
        FILE_ATTRIBUTE_DIRECTORY,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN,
        FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        NULL,
        0,
        0);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    PVOID temp = ExAllocatePool2(POOL_FLAG_NON_PAGED, 64 * 1024, AGENTFS_TAG);
    if (temp == NULL) {
        FltClose(handle);
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    BOOLEAN restartScan = TRUE;
    for (;;) {
        IO_STATUS_BLOCK queryStatus;
        RtlZeroMemory(&queryStatus, sizeof(queryStatus));
        RtlZeroMemory(temp, 64 * 1024);
        status = ZwQueryDirectoryFile(
            handle,
            NULL,
            NULL,
            NULL,
            &queryStatus,
            temp,
            64 * 1024,
            FileNamesInformation,
            FALSE,
            NULL,
            restartScan);
        restartScan = FALSE;
        if (status == STATUS_NO_MORE_FILES || status == STATUS_NO_SUCH_FILE) {
            status = STATUS_SUCCESS;
            break;
        }
        if (!NT_SUCCESS(status)) {
            break;
        }

        ULONG returned = (ULONG)queryStatus.Information;
        ULONG offset = 0;
        while (offset < returned) {
            PFILE_NAMES_INFORMATION entry = (PFILE_NAMES_INFORMATION)((PUCHAR)temp + offset);
            if (!AgentFsIsDotDirectoryName(entry->FileName, entry->FileNameLength)) {
                UNICODE_STRING sourceChild;
                UNICODE_STRING targetChild;
                RtlZeroMemory(&sourceChild, sizeof(sourceChild));
                RtlZeroMemory(&targetChild, sizeof(targetChild));
                status = AgentFsJoinChildPath(&sourceChild, Source, entry->FileName, entry->FileNameLength);
                if (NT_SUCCESS(status)) {
                    status = AgentFsJoinChildPath(&targetChild, Target, entry->FileName, entry->FileNameLength);
                }
                if (NT_SUCCESS(status)) {
                    if (AgentFsPathIsDirectory(Instance, &sourceChild)) {
                        status = AgentFsCopyDirectoryTree(Instance, &sourceChild, &targetChild);
                    } else {
                        status = AgentFsCopyFile(Instance, &sourceChild, &targetChild);
                    }
                }
                AgentFsFreeUnicode(&sourceChild);
                AgentFsFreeUnicode(&targetChild);
                if (!NT_SUCCESS(status)) {
                    goto Exit;
                }
            }

            if (entry->NextEntryOffset == 0) {
                break;
            }
            offset += entry->NextEntryOffset;
        }
    }

Exit:
    ExFreePoolWithTag(temp, AGENTFS_TAG);
    FltClose(handle);
    return status;
}

static NTSTATUS AgentFsDeleteDirectoryTree(_In_ PFLT_INSTANCE Instance, _In_ PCUNICODE_STRING Path)
{
    OBJECT_ATTRIBUTES oa;
    IO_STATUS_BLOCK iosb;
    HANDLE handle = NULL;
    InitializeObjectAttributes(&oa, (PUNICODE_STRING)Path, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    NTSTATUS status = FltCreateFile(
        gFilter,
        Instance,
        &handle,
        FILE_LIST_DIRECTORY | SYNCHRONIZE,
        &oa,
        &iosb,
        NULL,
        FILE_ATTRIBUTE_DIRECTORY,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN,
        FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        NULL,
        0,
        0);
    if (status == STATUS_OBJECT_NAME_NOT_FOUND || status == STATUS_OBJECT_PATH_NOT_FOUND) {
        return STATUS_SUCCESS;
    }
    if (!NT_SUCCESS(status)) {
        return status;
    }

    PVOID temp = ExAllocatePool2(POOL_FLAG_NON_PAGED, 64 * 1024, AGENTFS_TAG);
    if (temp == NULL) {
        FltClose(handle);
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    BOOLEAN restartScan = TRUE;
    for (;;) {
        IO_STATUS_BLOCK queryStatus;
        RtlZeroMemory(&queryStatus, sizeof(queryStatus));
        RtlZeroMemory(temp, 64 * 1024);
        status = ZwQueryDirectoryFile(
            handle,
            NULL,
            NULL,
            NULL,
            &queryStatus,
            temp,
            64 * 1024,
            FileNamesInformation,
            FALSE,
            NULL,
            restartScan);
        restartScan = FALSE;
        if (status == STATUS_NO_MORE_FILES || status == STATUS_NO_SUCH_FILE) {
            status = STATUS_SUCCESS;
            break;
        }
        if (!NT_SUCCESS(status)) {
            break;
        }

        ULONG returned = (ULONG)queryStatus.Information;
        ULONG offset = 0;
        while (offset < returned) {
            PFILE_NAMES_INFORMATION entry = (PFILE_NAMES_INFORMATION)((PUCHAR)temp + offset);
            if (!AgentFsIsDotDirectoryName(entry->FileName, entry->FileNameLength)) {
                UNICODE_STRING child;
                RtlZeroMemory(&child, sizeof(child));
                status = AgentFsJoinChildPath(&child, Path, entry->FileName, entry->FileNameLength);
                if (NT_SUCCESS(status)) {
                    if (AgentFsPathIsDirectory(Instance, &child)) {
                        status = AgentFsDeleteDirectoryTree(Instance, &child);
                    } else {
                        status = AgentFsDeletePath(&child);
                    }
                }
                AgentFsFreeUnicode(&child);
                if (!NT_SUCCESS(status)) {
                    goto Exit;
                }
            }

            if (entry->NextEntryOffset == 0) {
                break;
            }
            offset += entry->NextEntryOffset;
        }
    }

Exit:
    ExFreePoolWithTag(temp, AGENTFS_TAG);
    FltClose(handle);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    return AgentFsDeletePath(Path);
}

static ULONG AgentFsCreateDisposition(_In_ ULONG Options)
{
    return (Options >> 24) & 0xff;
}

static BOOLEAN AgentFsDispositionCanCreate(_In_ ULONG Disposition)
{
    return Disposition == FILE_CREATE ||
        Disposition == FILE_OPEN_IF ||
        Disposition == FILE_OVERWRITE_IF ||
        Disposition == FILE_SUPERSEDE;
}

static BOOLEAN AgentFsWriteIntent(_In_ ACCESS_MASK DesiredAccess, _In_ ULONG Options)
{
    ULONG disposition = AgentFsCreateDisposition(Options);
    if ((DesiredAccess & (
            FILE_WRITE_DATA |
            FILE_APPEND_DATA |
            FILE_WRITE_EA |
            FILE_WRITE_ATTRIBUTES |
            DELETE |
            WRITE_DAC |
            WRITE_OWNER)) != 0) {
        return TRUE;
    }
    return disposition == FILE_CREATE ||
        disposition == FILE_OVERWRITE ||
        disposition == FILE_OVERWRITE_IF ||
        disposition == FILE_SUPERSEDE;
}

static BOOLEAN AgentFsDirectoryOpen(_In_ ULONG Options)
{
    return (Options & FILE_DIRECTORY_FILE) != 0;
}

static BOOLEAN AgentFsDeleteRequested(_In_ PFLT_CALLBACK_DATA Data, _In_ FILE_INFORMATION_CLASS InfoClass)
{
    PVOID buffer = Data->Iopb->Parameters.SetFileInformation.InfoBuffer;
    if (buffer == NULL) {
        return FALSE;
    }
    if (InfoClass == FileDispositionInformation) {
        return ((PFILE_DISPOSITION_INFORMATION)buffer)->DeleteFile != FALSE;
    }
    if (InfoClass == FileDispositionInformationEx) {
        return (((PFILE_DISPOSITION_INFORMATION_EX)buffer)->Flags & FILE_DISPOSITION_DELETE) != 0;
    }
    return FALSE;
}

static BOOLEAN AgentFsPathExists(_In_ PFLT_INSTANCE Instance, _In_ PCUNICODE_STRING Path)
{
    OBJECT_ATTRIBUTES oa;
    IO_STATUS_BLOCK iosb;
    HANDLE handle = NULL;
    InitializeObjectAttributes(&oa, (PUNICODE_STRING)Path, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    NTSTATUS status = FltCreateFile(
        gFilter,
        Instance,
        &handle,
        FILE_READ_ATTRIBUTES,
        &oa,
        &iosb,
        NULL,
        FILE_ATTRIBUTE_NORMAL,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        NULL,
        0,
        0);
    if (NT_SUCCESS(status)) {
        FltClose(handle);
        return TRUE;
    }
    return FALSE;
}

static BOOLEAN AgentFsPathIsDirectory(_In_ PFLT_INSTANCE Instance, _In_ PCUNICODE_STRING Path)
{
    OBJECT_ATTRIBUTES oa;
    IO_STATUS_BLOCK iosb;
    HANDLE handle = NULL;
    InitializeObjectAttributes(&oa, (PUNICODE_STRING)Path, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    NTSTATUS status = FltCreateFile(
        gFilter,
        Instance,
        &handle,
        FILE_LIST_DIRECTORY | SYNCHRONIZE,
        &oa,
        &iosb,
        NULL,
        FILE_ATTRIBUTE_DIRECTORY,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN,
        FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        NULL,
        0,
        0);
    if (NT_SUCCESS(status)) {
        FltClose(handle);
        return TRUE;
    }
    return FALSE;
}

static BOOLEAN AgentFsDirectoryLayout(
    _In_ FILE_INFORMATION_CLASS InfoClass,
    _Out_ PULONG FileNameLengthOffset,
    _Out_ PULONG FileNameOffset)
{
    switch (InfoClass) {
    case FileDirectoryInformation:
        *FileNameLengthOffset = FIELD_OFFSET(FILE_DIRECTORY_INFORMATION, FileNameLength);
        *FileNameOffset = FIELD_OFFSET(FILE_DIRECTORY_INFORMATION, FileName);
        return TRUE;
    case FileFullDirectoryInformation:
        *FileNameLengthOffset = FIELD_OFFSET(FILE_FULL_DIR_INFORMATION, FileNameLength);
        *FileNameOffset = FIELD_OFFSET(FILE_FULL_DIR_INFORMATION, FileName);
        return TRUE;
    case FileBothDirectoryInformation:
        *FileNameLengthOffset = FIELD_OFFSET(FILE_BOTH_DIR_INFORMATION, FileNameLength);
        *FileNameOffset = FIELD_OFFSET(FILE_BOTH_DIR_INFORMATION, FileName);
        return TRUE;
    case FileIdFullDirectoryInformation:
        *FileNameLengthOffset = FIELD_OFFSET(FILE_ID_FULL_DIR_INFORMATION, FileNameLength);
        *FileNameOffset = FIELD_OFFSET(FILE_ID_FULL_DIR_INFORMATION, FileName);
        return TRUE;
    case FileIdBothDirectoryInformation:
        *FileNameLengthOffset = FIELD_OFFSET(FILE_ID_BOTH_DIR_INFORMATION, FileNameLength);
        *FileNameOffset = FIELD_OFFSET(FILE_ID_BOTH_DIR_INFORMATION, FileName);
        return TRUE;
    case FileNamesInformation:
        *FileNameLengthOffset = FIELD_OFFSET(FILE_NAMES_INFORMATION, FileNameLength);
        *FileNameOffset = FIELD_OFFSET(FILE_NAMES_INFORMATION, FileName);
        return TRUE;
    default:
        return FALSE;
    }
}

static ULONG AgentFsAlign8(_In_ ULONG Value)
{
    return (Value + 7) & ~7UL;
}

static ULONG AgentFsEntryRecordSize(
    _In_ PVOID Entry,
    _In_ ULONG Remaining,
    _In_ ULONG FileNameLengthOffset,
    _In_ ULONG FileNameOffset)
{
    ULONG next = *(PULONG)Entry;
    if (next != 0 && next <= Remaining) {
        return next;
    }
    ULONG nameLength = *(PULONG)((PUCHAR)Entry + FileNameLengthOffset);
    ULONG size = AgentFsAlign8(FileNameOffset + nameLength);
    return size <= Remaining ? size : Remaining;
}

static NTSTATUS AgentFsAppendDirectoryEntry(
    _Inout_updates_bytes_(Capacity) PVOID Output,
    _In_ ULONG Capacity,
    _Inout_ PULONG Used,
    _Inout_ PULONG LastOffset,
    _In_ PVOID Entry,
    _In_ ULONG EntrySize)
{
    EntrySize = AgentFsAlign8(EntrySize);
    if (*Used + EntrySize > Capacity) {
        return STATUS_BUFFER_OVERFLOW;
    }
    if (*Used != 0) {
        *(PULONG)((PUCHAR)Output + *LastOffset) = *Used - *LastOffset;
    }
    RtlCopyMemory((PUCHAR)Output + *Used, Entry, EntrySize);
    *(PULONG)((PUCHAR)Output + *Used) = 0;
    *LastOffset = *Used;
    *Used += EntrySize;
    return STATUS_SUCCESS;
}

static BOOLEAN AgentFsEntryHiddenByUpperOrWhiteout(
    _In_ PFLT_INSTANCE Instance,
    _In_ PAGENTFS_DIR_CONTEXT Context,
    _In_reads_bytes_(NameLength) PCWCH Name,
    _In_ ULONG NameLength,
    _In_ BOOLEAN CheckUpper)
{
    UNICODE_STRING child;
    RtlZeroMemory(&child, sizeof(child));
    NTSTATUS status = AgentFsJoinChildPath(&child, &Context->WhiteoutPath, Name, NameLength);
    if (NT_SUCCESS(status)) {
        BOOLEAN exists = AgentFsPathExists(Instance, &child);
        AgentFsFreeUnicode(&child);
        if (exists) {
            return TRUE;
        }
    }
    if (!CheckUpper) {
        return FALSE;
    }
    status = AgentFsJoinChildPath(&child, &Context->UpperPath, Name, NameLength);
    if (NT_SUCCESS(status)) {
        BOOLEAN exists = AgentFsPathExists(Instance, &child);
        AgentFsFreeUnicode(&child);
        if (exists) {
            return TRUE;
        }
    }
    return FALSE;
}

static NTSTATUS AgentFsSelectRedirect(
    _In_ PFLT_INSTANCE Instance,
    _In_ PAGENTFS_ENV Env,
    _In_ PCUNICODE_STRING OriginalPath,
    _In_ ACCESS_MASK DesiredAccess,
    _In_ ULONG Options,
    _Out_ PUNICODE_STRING Redirect)
{
    UNICODE_STRING whiteout;
    UNICODE_STRING upper;
    UNICODE_STRING lower;
    NTSTATUS status;
    RtlZeroMemory(&whiteout, sizeof(whiteout));
    RtlZeroMemory(&upper, sizeof(upper));
    RtlZeroMemory(&lower, sizeof(lower));

    status = AgentFsJoinRedirectPath(&whiteout, &Env->WhiteoutRoot, &Env->SourceRoot, OriginalPath);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    ULONG disposition = AgentFsCreateDisposition(Options);
    BOOLEAN writeIntent = AgentFsWriteIntent(DesiredAccess, Options);
    BOOLEAN deletedCreate = FALSE;
    if (AgentFsPathExists(Instance, &whiteout)) {
        if (!writeIntent || !AgentFsDispositionCanCreate(disposition)) {
            AgentFsFreeUnicode(&whiteout);
            return STATUS_OBJECT_NAME_NOT_FOUND;
        }
        status = AgentFsDeletePath(&whiteout);
        AgentFsFreeUnicode(&whiteout);
        if (!NT_SUCCESS(status)) {
            return status;
        }
        deletedCreate = TRUE;
    } else {
        AgentFsFreeUnicode(&whiteout);
    }

    status = AgentFsJoinRedirectPath(&upper, &Env->UpperRoot, &Env->SourceRoot, OriginalPath);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    BOOLEAN directoryRead = AgentFsDirectoryOpen(Options) && !writeIntent;
    if (writeIntent && !AgentFsPathExists(Instance, &upper)) {
        if (deletedCreate) {
            status = AgentFsEnsureParentDirectory(Instance, &upper);
            if (!NT_SUCCESS(status)) {
                AgentFsFreeUnicode(&upper);
                return status;
            }
        } else {
            status = AgentFsJoinRedirectPath(&lower, &Env->LowerRoot, &Env->SourceRoot, OriginalPath);
            if (!NT_SUCCESS(status)) {
                AgentFsFreeUnicode(&upper);
                return status;
            }
            if (AgentFsPathExists(Instance, &lower)) {
                if (AgentFsDirectoryOpen(Options) || AgentFsPathIsDirectory(Instance, &lower)) {
                    status = AgentFsEnsureDirectoryTree(Instance, &upper);
                } else {
                    status = AgentFsCopyFile(Instance, &lower, &upper);
                }
                AgentFsFreeUnicode(&lower);
                if (!NT_SUCCESS(status)) {
                    AgentFsFreeUnicode(&upper);
                    return status;
                }
            } else {
                AgentFsFreeUnicode(&lower);
                status = AgentFsEnsureParentDirectory(Instance, &upper);
                if (!NT_SUCCESS(status)) {
                    AgentFsFreeUnicode(&upper);
                    return status;
                }
            }
        }
    }
    if (!directoryRead && (writeIntent || AgentFsPathExists(Instance, &upper))) {
        *Redirect = upper;
        return STATUS_SUCCESS;
    }

    status = AgentFsJoinRedirectPath(&lower, &Env->LowerRoot, &Env->SourceRoot, OriginalPath);
    if (!NT_SUCCESS(status)) {
        AgentFsFreeUnicode(&upper);
        return status;
    }
    if (AgentFsPathExists(Instance, &lower)) {
        AgentFsFreeUnicode(&upper);
        *Redirect = lower;
        return STATUS_SUCCESS;
    }
    if (directoryRead && AgentFsPathExists(Instance, &upper)) {
        AgentFsFreeUnicode(&lower);
        *Redirect = upper;
        return STATUS_SUCCESS;
    }
    AgentFsFreeUnicode(&upper);
    AgentFsFreeUnicode(&lower);
    return STATUS_OBJECT_NAME_NOT_FOUND;
}

static PAGENTFS_ENV AgentFsFindEnvLocked(_In_ HANDLE ProcessId)
{
    for (PLIST_ENTRY link = gEnvs.Flink; link != &gEnvs; link = link->Flink) {
        PAGENTFS_ENV env = CONTAINING_RECORD(link, AGENTFS_ENV, Link);
        if (env->ProcessId == ProcessId) {
            return env;
        }
    }
    return NULL;
}

static NTSTATUS AgentFsDupUnicode(_Out_ PUNICODE_STRING Destination, _In_ PCUNICODE_STRING Source)
{
    RtlZeroMemory(Destination, sizeof(*Destination));
    Destination->Length = Source->Length;
    Destination->MaximumLength = Source->Length + sizeof(WCHAR);
    Destination->Buffer = ExAllocatePool2(POOL_FLAG_NON_PAGED, Destination->MaximumLength, AGENTFS_TAG);
    if (Destination->Buffer == NULL) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    RtlCopyMemory(Destination->Buffer, Source->Buffer, Source->Length);
    Destination->Buffer[Source->Length / sizeof(WCHAR)] = L'\0';
    return STATUS_SUCCESS;
}

static NTSTATUS AgentFsCloneEnvForProcessLocked(
    _In_ PAGENTFS_ENV Parent,
    _In_ HANDLE ProcessId,
    _In_ BOOLEAN Insert,
    _Outptr_ PAGENTFS_ENV *Child)
{
    PAGENTFS_ENV env = ExAllocatePool2(POOL_FLAG_NON_PAGED, sizeof(AGENTFS_ENV), AGENTFS_TAG);
    if (env == NULL) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    RtlZeroMemory(env, sizeof(*env));
    env->ProcessId = ProcessId;
    NTSTATUS status = AgentFsDupUnicode(&env->EnvId, &Parent->EnvId);
    if (NT_SUCCESS(status)) status = AgentFsDupUnicode(&env->SourceRoot, &Parent->SourceRoot);
    if (NT_SUCCESS(status)) status = AgentFsDupUnicode(&env->LowerRoot, &Parent->LowerRoot);
    if (NT_SUCCESS(status)) status = AgentFsDupUnicode(&env->UpperRoot, &Parent->UpperRoot);
    if (NT_SUCCESS(status)) status = AgentFsDupUnicode(&env->WhiteoutRoot, &Parent->WhiteoutRoot);
    if (!NT_SUCCESS(status)) {
        AgentFsFreeEnv(env);
        return status;
    }
    if (Insert) {
        InsertTailList(&gEnvs, &env->Link);
    }
    *Child = env;
    return STATUS_SUCCESS;
}

static NTSTATUS AgentFsSnapshotEnvForProcess(_In_ HANDLE ProcessId, _Outptr_result_maybenull_ PAGENTFS_ENV *Snapshot)
{
    PAGENTFS_ENV env;
    *Snapshot = NULL;
    ExAcquireFastMutex(&gEnvLock);
    env = AgentFsFindEnvLocked(ProcessId);
    if (env != NULL) {
        (VOID)AgentFsCloneEnvForProcessLocked(env, ProcessId, FALSE, Snapshot);
    }
    ExReleaseFastMutex(&gEnvLock);
    return *Snapshot == NULL ? STATUS_NOT_FOUND : STATUS_SUCCESS;
}

static FLT_PREOP_CALLBACK_STATUS AgentFsPreCreate(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _Flt_CompletionContext_Outptr_ PVOID *CompletionContext)
{
    UNREFERENCED_PARAMETER(CompletionContext);
    HANDLE pid = PsGetCurrentProcessId();
    PFLT_FILE_NAME_INFORMATION nameInfo = NULL;
    PAGENTFS_ENV env = NULL;
    UNICODE_STRING redirect;
    NTSTATUS status;
    RtlZeroMemory(&redirect, sizeof(redirect));

    status = AgentFsSnapshotEnvForProcess(pid, &env);
    if (!NT_SUCCESS(status)) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    status = FltGetFileNameInformation(
        Data,
        FLT_FILE_NAME_NORMALIZED | FLT_FILE_NAME_QUERY_DEFAULT,
        &nameInfo);
    if (!NT_SUCCESS(status)) {
        AgentFsFreeEnv(env);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }
    status = FltParseFileNameInformation(nameInfo);
    if (!NT_SUCCESS(status) || !AgentFsStartsWithPath(&nameInfo->Name, &env->SourceRoot)) {
        FltReleaseFileNameInformation(nameInfo);
        AgentFsFreeEnv(env);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    status = AgentFsSelectRedirect(
        FltObjects->Instance,
        env,
        &nameInfo->Name,
        Data->Iopb->Parameters.Create.SecurityContext->DesiredAccess,
        Data->Iopb->Parameters.Create.Options,
        &redirect);
    FltReleaseFileNameInformation(nameInfo);
    AgentFsFreeEnv(env);

    if (status == STATUS_OBJECT_NAME_NOT_FOUND) {
        Data->IoStatus.Status = status;
        Data->IoStatus.Information = 0;
        return FLT_PREOP_COMPLETE;
    }
    if (!NT_SUCCESS(status)) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    IoReplaceFileObjectName(
        Data->Iopb->TargetFileObject,
        redirect.Buffer,
        redirect.Length);
    AgentFsFreeUnicode(&redirect);
    FltSetCallbackDataDirty(Data);
    Data->IoStatus.Status = STATUS_REPARSE;
    Data->IoStatus.Information = IO_REPARSE;
    return FLT_PREOP_COMPLETE;
}

static NTSTATUS AgentFsVisiblePathFromName(
    _In_ PAGENTFS_ENV Env,
    _In_ PCUNICODE_STRING Name,
    _Out_ PUNICODE_STRING Visible)
{
    if (AgentFsStartsWithPath(Name, &Env->SourceRoot)) {
        return AgentFsDupUnicode(Visible, Name);
    }
    NTSTATUS status = AgentFsStorageToVisiblePath(Visible, &Env->SourceRoot, &Env->UpperRoot, Name);
    if (NT_SUCCESS(status)) {
        return status;
    }
    return AgentFsStorageToVisiblePath(Visible, &Env->SourceRoot, &Env->LowerRoot, Name);
}

static FLT_PREOP_CALLBACK_STATUS AgentFsPreFileSystemControl(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _Flt_CompletionContext_Outptr_ PVOID *CompletionContext)
{
    UNREFERENCED_PARAMETER(CompletionContext);
    UNREFERENCED_PARAMETER(FltObjects);
    if (Data->Iopb->MinorFunction != IRP_MN_USER_FS_REQUEST ||
        Data->Iopb->Parameters.FileSystemControl.Common.FsControlCode != FSCTL_SET_REPARSE_POINT) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    HANDLE pid = PsGetCurrentProcessId();
    PAGENTFS_ENV env = NULL;
    PFLT_FILE_NAME_INFORMATION nameInfo = NULL;
    UNICODE_STRING visible;
    RtlZeroMemory(&visible, sizeof(visible));

    NTSTATUS status = AgentFsSnapshotEnvForProcess(pid, &env);
    if (!NT_SUCCESS(status)) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }
    status = FltGetFileNameInformation(
        Data,
        FLT_FILE_NAME_NORMALIZED | FLT_FILE_NAME_QUERY_DEFAULT,
        &nameInfo);
    if (!NT_SUCCESS(status)) {
        AgentFsFreeEnv(env);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }
    status = FltParseFileNameInformation(nameInfo);
    if (NT_SUCCESS(status)) {
        status = AgentFsVisiblePathFromName(env, &nameInfo->Name, &visible);
    }
    FltReleaseFileNameInformation(nameInfo);
    if (!NT_SUCCESS(status)) {
        AgentFsFreeEnv(env);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    BOOLEAN managed = AgentFsStartsWithPath(&visible, &env->SourceRoot);
    AgentFsFreeUnicode(&visible);
    AgentFsFreeEnv(env);
    if (!managed) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    Data->IoStatus.Status = STATUS_NOT_SUPPORTED;
    Data->IoStatus.Information = 0;
    return FLT_PREOP_COMPLETE;
}

static FLT_PREOP_CALLBACK_STATUS AgentFsPreSetInformation(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _Flt_CompletionContext_Outptr_ PVOID *CompletionContext)
{
    UNREFERENCED_PARAMETER(CompletionContext);
    FILE_INFORMATION_CLASS infoClass = Data->Iopb->Parameters.SetFileInformation.FileInformationClass;
    if (infoClass != FileDispositionInformation &&
        infoClass != FileDispositionInformationEx &&
        infoClass != FileRenameInformation &&
        infoClass != FileRenameInformationEx &&
        infoClass != FileLinkInformation &&
        infoClass != FileLinkInformationEx) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }
    if ((infoClass == FileDispositionInformation || infoClass == FileDispositionInformationEx) &&
        !AgentFsDeleteRequested(Data, infoClass)) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    HANDLE pid = PsGetCurrentProcessId();
    PAGENTFS_ENV env = NULL;
    PFLT_FILE_NAME_INFORMATION nameInfo = NULL;
    NTSTATUS status;

    status = AgentFsSnapshotEnvForProcess(pid, &env);
    if (!NT_SUCCESS(status)) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }
    status = FltGetFileNameInformation(
        Data,
        FLT_FILE_NAME_NORMALIZED | FLT_FILE_NAME_QUERY_DEFAULT,
        &nameInfo);
    if (!NT_SUCCESS(status)) {
        AgentFsFreeEnv(env);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }
    status = FltParseFileNameInformation(nameInfo);
    if (!NT_SUCCESS(status)) {
        FltReleaseFileNameInformation(nameInfo);
        AgentFsFreeEnv(env);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    UNICODE_STRING visible;
    UNICODE_STRING whiteout;
    UNICODE_STRING upper;
    UNICODE_STRING lower;
    RtlZeroMemory(&visible, sizeof(visible));
    RtlZeroMemory(&whiteout, sizeof(whiteout));
    RtlZeroMemory(&upper, sizeof(upper));
    RtlZeroMemory(&lower, sizeof(lower));
    status = AgentFsVisiblePathFromName(env, &nameInfo->Name, &visible);
    FltReleaseFileNameInformation(nameInfo);
    if (!NT_SUCCESS(status)) {
        AgentFsFreeEnv(env);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    if (infoClass == FileRenameInformation || infoClass == FileRenameInformationEx) {
        PFILE_RENAME_INFORMATION renameInfo =
            (PFILE_RENAME_INFORMATION)Data->Iopb->Parameters.SetFileInformation.InfoBuffer;
        UNICODE_STRING targetVisible;
        UNICODE_STRING targetUpper;
        UNICODE_STRING targetLower;
        UNICODE_STRING targetWhiteout;
        RtlZeroMemory(&targetVisible, sizeof(targetVisible));
        RtlZeroMemory(&targetUpper, sizeof(targetUpper));
        RtlZeroMemory(&targetLower, sizeof(targetLower));
        RtlZeroMemory(&targetWhiteout, sizeof(targetWhiteout));
        if (renameInfo == NULL || renameInfo->RootDirectory != NULL) {
            status = STATUS_NOT_SUPPORTED;
        } else {
            status = AgentFsSiblingVisiblePath(
                &targetVisible,
                &visible,
                renameInfo->FileName,
                renameInfo->FileNameLength);
        }
        if (NT_SUCCESS(status) && !AgentFsStartsWithPath(&targetVisible, &env->SourceRoot)) {
            status = STATUS_NOT_SUPPORTED;
        }
        if (NT_SUCCESS(status)) {
            status = AgentFsJoinRedirectPath(&upper, &env->UpperRoot, &env->SourceRoot, &visible);
        }
        if (NT_SUCCESS(status)) {
            status = AgentFsJoinRedirectPath(&lower, &env->LowerRoot, &env->SourceRoot, &visible);
        }
        if (NT_SUCCESS(status)) {
            status = AgentFsJoinRedirectPath(&targetUpper, &env->UpperRoot, &env->SourceRoot, &targetVisible);
        }
        if (NT_SUCCESS(status)) {
            status = AgentFsJoinRedirectPath(&targetLower, &env->LowerRoot, &env->SourceRoot, &targetVisible);
        }
        if (NT_SUCCESS(status)) {
            status = AgentFsJoinRedirectPath(&targetWhiteout, &env->WhiteoutRoot, &env->SourceRoot, &targetVisible);
        }
        if (NT_SUCCESS(status) &&
            renameInfo->ReplaceIfExists == FALSE &&
            !AgentFsPathExists(FltObjects->Instance, &targetWhiteout) &&
            (AgentFsPathExists(FltObjects->Instance, &targetUpper) ||
                AgentFsPathExists(FltObjects->Instance, &targetLower))) {
            status = STATUS_OBJECT_NAME_COLLISION;
        }
        if (NT_SUCCESS(status)) {
            if (AgentFsPathExists(FltObjects->Instance, &upper)) {
                status = AgentFsRenamePath(
                    FltObjects->Instance,
                    &upper,
                    &targetUpper,
                    renameInfo->ReplaceIfExists != FALSE);
                if (NT_SUCCESS(status) && AgentFsPathIsDirectory(FltObjects->Instance, &lower)) {
                    status = AgentFsCopyDirectoryTree(FltObjects->Instance, &lower, &targetUpper);
                }
            } else if (AgentFsPathIsDirectory(FltObjects->Instance, &lower)) {
                status = AgentFsCopyDirectoryTree(FltObjects->Instance, &lower, &targetUpper);
            } else {
                status = AgentFsCopyFile(FltObjects->Instance, &lower, &targetUpper);
            }
        }
        if (NT_SUCCESS(status)) {
            status = AgentFsDeletePath(&targetWhiteout);
        }
        if (NT_SUCCESS(status)) {
            status = AgentFsJoinRedirectPath(&whiteout, &env->WhiteoutRoot, &env->SourceRoot, &visible);
        }
        if (NT_SUCCESS(status)) {
            status = AgentFsCreateEmptyFile(FltObjects->Instance, &whiteout);
        }
        AgentFsFreeUnicode(&targetVisible);
        AgentFsFreeUnicode(&targetUpper);
        AgentFsFreeUnicode(&targetLower);
        AgentFsFreeUnicode(&targetWhiteout);
        AgentFsFreeUnicode(&upper);
        AgentFsFreeUnicode(&lower);
        AgentFsFreeUnicode(&visible);
        AgentFsFreeEnv(env);
        Data->IoStatus.Status = status;
        Data->IoStatus.Information = 0;
        return FLT_PREOP_COMPLETE;
    }

    if (infoClass == FileLinkInformation || infoClass == FileLinkInformationEx) {
        AgentFsFreeUnicode(&visible);
        AgentFsFreeEnv(env);
        Data->IoStatus.Status = STATUS_NOT_SUPPORTED;
        Data->IoStatus.Information = 0;
        return FLT_PREOP_COMPLETE;
    }

    status = AgentFsJoinRedirectPath(&upper, &env->UpperRoot, &env->SourceRoot, &visible);
    if (NT_SUCCESS(status)) {
        if (AgentFsPathIsDirectory(FltObjects->Instance, &upper)) {
            status = AgentFsDeleteDirectoryTree(FltObjects->Instance, &upper);
        } else {
            status = AgentFsDeletePath(&upper);
        }
    }
    AgentFsFreeUnicode(&upper);
    if (!NT_SUCCESS(status)) {
        AgentFsFreeUnicode(&visible);
        AgentFsFreeEnv(env);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }
    status = AgentFsJoinRedirectPath(&whiteout, &env->WhiteoutRoot, &env->SourceRoot, &visible);
    AgentFsFreeUnicode(&visible);
    AgentFsFreeEnv(env);
    if (!NT_SUCCESS(status)) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }
    status = AgentFsCreateEmptyFile(FltObjects->Instance, &whiteout);
    AgentFsFreeUnicode(&whiteout);
    Data->IoStatus.Status = status;
    Data->IoStatus.Information = 0;
    return FLT_PREOP_COMPLETE;
}

static FLT_PREOP_CALLBACK_STATUS AgentFsPreDirectoryControl(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _Flt_CompletionContext_Outptr_ PVOID *CompletionContext)
{
    UNREFERENCED_PARAMETER(FltObjects);
    *CompletionContext = NULL;
    if (Data->Iopb->MinorFunction != IRP_MN_QUERY_DIRECTORY) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }
    if ((Data->Iopb->OperationFlags & SL_RESTART_SCAN) != 0 ||
        Data->Iopb->Parameters.DirectoryControl.QueryDirectory.FileName != NULL) {
        AgentFsResetDirState(FltObjects->FileObject);
    }

    HANDLE pid = PsGetCurrentProcessId();
    PAGENTFS_ENV env = NULL;
    PFLT_FILE_NAME_INFORMATION nameInfo = NULL;
    PAGENTFS_DIR_CONTEXT context = NULL;
    NTSTATUS status;

    status = AgentFsSnapshotEnvForProcess(pid, &env);
    if (!NT_SUCCESS(status)) {
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }
    status = FltGetFileNameInformation(
        Data,
        FLT_FILE_NAME_NORMALIZED | FLT_FILE_NAME_QUERY_DEFAULT,
        &nameInfo);
    if (!NT_SUCCESS(status)) {
        AgentFsFreeEnv(env);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }
    status = FltParseFileNameInformation(nameInfo);
    if (!NT_SUCCESS(status)) {
        FltReleaseFileNameInformation(nameInfo);
        AgentFsFreeEnv(env);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }

    context = ExAllocatePool2(POOL_FLAG_NON_PAGED, sizeof(AGENTFS_DIR_CONTEXT), AGENTFS_TAG);
    if (context == NULL) {
        FltReleaseFileNameInformation(nameInfo);
        AgentFsFreeEnv(env);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }
    RtlZeroMemory(context, sizeof(*context));
    context->EnumeratingUpperPath = AgentFsStartsWithPath(&nameInfo->Name, &env->UpperRoot);
    status = AgentFsVisiblePathFromName(env, &nameInfo->Name, &context->VisiblePath);
    FltReleaseFileNameInformation(nameInfo);
    if (NT_SUCCESS(status)) {
        status = AgentFsJoinRedirectPath(&context->LowerPath, &env->LowerRoot, &env->SourceRoot, &context->VisiblePath);
    }
    if (NT_SUCCESS(status)) {
        status = AgentFsJoinRedirectPath(&context->UpperPath, &env->UpperRoot, &env->SourceRoot, &context->VisiblePath);
    }
    if (NT_SUCCESS(status)) {
        status = AgentFsJoinRedirectPath(&context->WhiteoutPath, &env->WhiteoutRoot, &env->SourceRoot, &context->VisiblePath);
    }
    AgentFsFreeEnv(env);
    if (!NT_SUCCESS(status)) {
        AgentFsFreeDirContext(context);
        return FLT_PREOP_SUCCESS_NO_CALLBACK;
    }
    *CompletionContext = context;
    return FLT_PREOP_SUCCESS_WITH_CALLBACK;
}

static PVOID AgentFsDirectoryBufferAddress(_In_ PFLT_CALLBACK_DATA Data)
{
    if (Data->Iopb->Parameters.DirectoryControl.QueryDirectory.MdlAddress != NULL) {
        return MmGetSystemAddressForMdlSafe(
            Data->Iopb->Parameters.DirectoryControl.QueryDirectory.MdlAddress,
            NormalPagePriority | MdlMappingNoExecute);
    }
    return Data->Iopb->Parameters.DirectoryControl.QueryDirectory.DirectoryBuffer;
}

static PAGENTFS_DIR_STATE AgentFsFindDirStateLocked(_In_ PFILE_OBJECT FileObject)
{
    for (PLIST_ENTRY link = gDirStates.Flink; link != &gDirStates; link = link->Flink) {
        PAGENTFS_DIR_STATE state = CONTAINING_RECORD(link, AGENTFS_DIR_STATE, Link);
        if (state->FileObject == FileObject) {
            return state;
        }
    }
    return NULL;
}

static VOID AgentFsResetDirState(_In_ PFILE_OBJECT FileObject)
{
    if (FileObject == NULL) {
        return;
    }
    ExAcquireFastMutex(&gDirStateLock);
    PAGENTFS_DIR_STATE state = AgentFsFindDirStateLocked(FileObject);
    if (state != NULL) {
        state->UpperMerged = FALSE;
    }
    ExReleaseFastMutex(&gDirStateLock);
}

static BOOLEAN AgentFsDirUpperAlreadyMerged(_In_ PFILE_OBJECT FileObject)
{
    if (FileObject == NULL) {
        return FALSE;
    }
    ExAcquireFastMutex(&gDirStateLock);
    PAGENTFS_DIR_STATE state = AgentFsFindDirStateLocked(FileObject);
    BOOLEAN merged = state != NULL && state->UpperMerged;
    ExReleaseFastMutex(&gDirStateLock);
    return merged;
}

static VOID AgentFsMarkDirUpperMerged(_In_ PFILE_OBJECT FileObject)
{
    if (FileObject == NULL) {
        return;
    }
    ExAcquireFastMutex(&gDirStateLock);
    PAGENTFS_DIR_STATE state = AgentFsFindDirStateLocked(FileObject);
    if (state == NULL) {
        state = ExAllocatePool2(POOL_FLAG_NON_PAGED, sizeof(AGENTFS_DIR_STATE), AGENTFS_TAG);
        if (state != NULL) {
            RtlZeroMemory(state, sizeof(*state));
            state->FileObject = FileObject;
            InsertTailList(&gDirStates, &state->Link);
        }
    }
    if (state != NULL) {
        state->UpperMerged = TRUE;
    }
    ExReleaseFastMutex(&gDirStateLock);
}

static VOID AgentFsRemoveDirState(_In_opt_ PFILE_OBJECT FileObject)
{
    if (FileObject == NULL) {
        return;
    }
    ExAcquireFastMutex(&gDirStateLock);
    PAGENTFS_DIR_STATE state = AgentFsFindDirStateLocked(FileObject);
    if (state != NULL) {
        RemoveEntryList(&state->Link);
    }
    ExReleaseFastMutex(&gDirStateLock);
    if (state != NULL) {
        ExFreePoolWithTag(state, AGENTFS_TAG);
    }
}

static NTSTATUS AgentFsAppendUpperDirectoryEntries(
    _In_ PFLT_INSTANCE Instance,
    _In_ PAGENTFS_DIR_CONTEXT Context,
    _In_ FILE_INFORMATION_CLASS InfoClass,
    _In_opt_ PUNICODE_STRING SearchPattern,
    _In_ ULONG FileNameLengthOffset,
    _In_ ULONG FileNameOffset,
    _Inout_updates_bytes_(Capacity) PVOID Output,
    _In_ ULONG Capacity,
    _Inout_ PULONG Used,
    _Inout_ PULONG LastOffset)
{
    if (!AgentFsPathExists(Instance, &Context->UpperPath)) {
        return STATUS_SUCCESS;
    }

    OBJECT_ATTRIBUTES oa;
    IO_STATUS_BLOCK iosb;
    HANDLE handle = NULL;
    InitializeObjectAttributes(&oa, &Context->UpperPath, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    NTSTATUS status = FltCreateFile(
        gFilter,
        Instance,
        &handle,
        FILE_LIST_DIRECTORY | SYNCHRONIZE,
        &oa,
        &iosb,
        NULL,
        FILE_ATTRIBUTE_DIRECTORY,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN,
        FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
        NULL,
        0,
        0);
    if (!NT_SUCCESS(status)) {
        return STATUS_SUCCESS;
    }

    PVOID temp = ExAllocatePool2(POOL_FLAG_NON_PAGED, 64 * 1024, AGENTFS_TAG);
    if (temp == NULL) {
        FltClose(handle);
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    BOOLEAN restartScan = TRUE;
    for (;;) {
        IO_STATUS_BLOCK queryStatus;
        RtlZeroMemory(&queryStatus, sizeof(queryStatus));
        RtlZeroMemory(temp, 64 * 1024);
        status = ZwQueryDirectoryFile(
            handle,
            NULL,
            NULL,
            NULL,
            &queryStatus,
            temp,
            64 * 1024,
            InfoClass,
            FALSE,
            SearchPattern,
            restartScan);
        restartScan = FALSE;
        if (status == STATUS_NO_MORE_FILES || status == STATUS_NO_SUCH_FILE) {
            status = STATUS_SUCCESS;
            break;
        }
        if (!NT_SUCCESS(status)) {
            break;
        }
        ULONG offset = 0;
        ULONG returned = (ULONG)queryStatus.Information;
        while (offset < returned) {
            PVOID entry = (PUCHAR)temp + offset;
            ULONG remaining = returned - offset;
            ULONG entrySize = AgentFsEntryRecordSize(entry, remaining, FileNameLengthOffset, FileNameOffset);
            ULONG nameLength = *(PULONG)((PUCHAR)entry + FileNameLengthOffset);
            PCWCH name = (PCWCH)((PUCHAR)entry + FileNameOffset);
            if (!AgentFsEntryHiddenByUpperOrWhiteout(Instance, Context, name, nameLength, FALSE)) {
                NTSTATUS appendStatus = AgentFsAppendDirectoryEntry(Output, Capacity, Used, LastOffset, entry, entrySize);
                if (!NT_SUCCESS(appendStatus)) {
                    status = STATUS_SUCCESS;
                    goto Exit;
                }
            }
            ULONG next = *(PULONG)entry;
            if (next == 0) {
                break;
            }
            offset += next;
        }
    }

Exit:
    ExFreePoolWithTag(temp, AGENTFS_TAG);
    FltClose(handle);
    return status;
}

static BOOLEAN AgentFsShouldAppendUpperDirectoryEntries(_In_ PFLT_CALLBACK_DATA Data)
{
    return Data->IoStatus.Status == STATUS_NO_MORE_FILES;
}

static FLT_POSTOP_CALLBACK_STATUS AgentFsPostDirectoryControl(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _In_opt_ PVOID CompletionContext,
    _In_ FLT_POST_OPERATION_FLAGS Flags)
{
    UNREFERENCED_PARAMETER(Flags);
    PAGENTFS_DIR_CONTEXT context = (PAGENTFS_DIR_CONTEXT)CompletionContext;
    if (context == NULL) {
        return FLT_POSTOP_FINISHED_PROCESSING;
    }
    if (!NT_SUCCESS(Data->IoStatus.Status) && Data->IoStatus.Status != STATUS_NO_MORE_FILES) {
        AgentFsFreeDirContext(context);
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    FILE_INFORMATION_CLASS infoClass = Data->Iopb->Parameters.DirectoryControl.QueryDirectory.FileInformationClass;
    ULONG nameLengthOffset = 0;
    ULONG nameOffset = 0;
    if (!AgentFsDirectoryLayout(infoClass, &nameLengthOffset, &nameOffset)) {
        AgentFsFreeDirContext(context);
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    ULONG capacity = Data->Iopb->Parameters.DirectoryControl.QueryDirectory.Length;
    PVOID userBuffer = AgentFsDirectoryBufferAddress(Data);
    if (userBuffer == NULL || capacity == 0) {
        AgentFsFreeDirContext(context);
        return FLT_POSTOP_FINISHED_PROCESSING;
    }

    PVOID output = ExAllocatePool2(POOL_FLAG_NON_PAGED, capacity, AGENTFS_TAG);
    if (output == NULL) {
        AgentFsFreeDirContext(context);
        return FLT_POSTOP_FINISHED_PROCESSING;
    }
    RtlZeroMemory(output, capacity);

    ULONG used = 0;
    ULONG lastOffset = 0;
    ULONG inputUsed = (ULONG)Data->IoStatus.Information;
    if (NT_SUCCESS(Data->IoStatus.Status) && inputUsed > 0) {
        ULONG offset = 0;
        while (offset < inputUsed) {
            PVOID entry = (PUCHAR)userBuffer + offset;
            ULONG remaining = inputUsed - offset;
            ULONG entrySize = AgentFsEntryRecordSize(entry, remaining, nameLengthOffset, nameOffset);
            ULONG nameLength = *(PULONG)((PUCHAR)entry + nameLengthOffset);
            PCWCH name = (PCWCH)((PUCHAR)entry + nameOffset);
            if (!AgentFsEntryHiddenByUpperOrWhiteout(
                    FltObjects->Instance,
                    context,
                    name,
                    nameLength,
                    !context->EnumeratingUpperPath)) {
                if (!NT_SUCCESS(AgentFsAppendDirectoryEntry(output, capacity, &used, &lastOffset, entry, entrySize))) {
                    break;
                }
            }
            ULONG next = *(PULONG)entry;
            if (next == 0) {
                break;
            }
            offset += next;
        }
    }

    if (!context->EnumeratingUpperPath && AgentFsShouldAppendUpperDirectoryEntries(Data)) {
        if (!AgentFsDirUpperAlreadyMerged(FltObjects->FileObject)) {
            (VOID)AgentFsAppendUpperDirectoryEntries(
                FltObjects->Instance,
                context,
                infoClass,
                Data->Iopb->Parameters.DirectoryControl.QueryDirectory.FileName,
                nameLengthOffset,
                nameOffset,
                output,
                capacity,
                &used,
                &lastOffset);
            AgentFsMarkDirUpperMerged(FltObjects->FileObject);
        }
    }

    if (used == 0) {
        Data->IoStatus.Status = STATUS_NO_MORE_FILES;
        Data->IoStatus.Information = 0;
    } else {
        RtlCopyMemory(userBuffer, output, used);
        Data->IoStatus.Status = STATUS_SUCCESS;
        Data->IoStatus.Information = used;
    }

    ExFreePoolWithTag(output, AGENTFS_TAG);
    AgentFsFreeDirContext(context);
    return FLT_POSTOP_FINISHED_PROCESSING;
}

static FLT_PREOP_CALLBACK_STATUS AgentFsPreCleanup(
    _Inout_ PFLT_CALLBACK_DATA Data,
    _In_ PCFLT_RELATED_OBJECTS FltObjects,
    _Flt_CompletionContext_Outptr_ PVOID *CompletionContext)
{
    UNREFERENCED_PARAMETER(Data);
    UNREFERENCED_PARAMETER(CompletionContext);
    AgentFsRemoveDirState(FltObjects->FileObject);
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
}

static NTSTATUS AgentFsRegister(_In_ PAGENTFS_REQUEST Request)
{
    PAGENTFS_ENV env = ExAllocatePool2(POOL_FLAG_NON_PAGED, sizeof(AGENTFS_ENV), AGENTFS_TAG);
    if (env == NULL) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    RtlZeroMemory(env, sizeof(*env));
    env->ProcessId = ULongToHandle(Request->ProcessId);
    NTSTATUS status = AgentFsDupUserString(&env->EnvId, Request->EnvId, AGENTFS_ENV_ID_CHARS);
    if (NT_SUCCESS(status)) status = AgentFsDupUserString(&env->SourceRoot, Request->SourceRoot, AGENTFS_MAX_CHARS);
    if (NT_SUCCESS(status)) status = AgentFsDupUserString(&env->LowerRoot, Request->LowerRoot, AGENTFS_MAX_CHARS);
    if (NT_SUCCESS(status)) status = AgentFsDupUserString(&env->UpperRoot, Request->UpperRoot, AGENTFS_MAX_CHARS);
    if (NT_SUCCESS(status)) status = AgentFsDupUserString(&env->WhiteoutRoot, Request->WhiteoutRoot, AGENTFS_MAX_CHARS);
    if (!NT_SUCCESS(status)) {
        AgentFsFreeEnv(env);
        return status;
    }

    ExAcquireFastMutex(&gEnvLock);
    PAGENTFS_ENV old = AgentFsFindEnvLocked(env->ProcessId);
    if (old != NULL) {
        RemoveEntryList(&old->Link);
        AgentFsFreeEnv(old);
    }
    InsertTailList(&gEnvs, &env->Link);
    ExReleaseFastMutex(&gEnvLock);
    return STATUS_SUCCESS;
}

static VOID AgentFsUnregister(_In_ ULONG ProcessId)
{
    ExAcquireFastMutex(&gEnvLock);
    PAGENTFS_ENV env = AgentFsFindEnvLocked(ULongToHandle(ProcessId));
    if (env != NULL) {
        RemoveEntryList(&env->Link);
    }
    ExReleaseFastMutex(&gEnvLock);
    if (env != NULL) {
        AgentFsFreeEnv(env);
    }
}

static VOID AgentFsProcessNotify(
    _Inout_ PEPROCESS Process,
    _In_ HANDLE ProcessId,
    _Inout_opt_ PPS_CREATE_NOTIFY_INFO CreateInfo)
{
    UNREFERENCED_PARAMETER(Process);
    if (CreateInfo == NULL) {
        AgentFsUnregister(HandleToULong(ProcessId));
        return;
    }
    ExAcquireFastMutex(&gEnvLock);
    if (AgentFsFindEnvLocked(ProcessId) == NULL) {
        PAGENTFS_ENV parent = AgentFsFindEnvLocked(CreateInfo->ParentProcessId);
        if (parent != NULL) {
            PAGENTFS_ENV child = NULL;
            (VOID)AgentFsCloneEnvForProcessLocked(parent, ProcessId, TRUE, &child);
        }
    }
    ExReleaseFastMutex(&gEnvLock);
}

static VOID AgentFsWriteReply(_Out_ PAGENTFS_REPLY Reply, _In_ ULONG Status, _In_ NTSTATUS NtStatus)
{
    RtlZeroMemory(Reply, sizeof(*Reply));
    Reply->Status = Status;
    Reply->Win32Error = RtlNtStatusToDosError(NtStatus);
}

static NTSTATUS AgentFsConnect(
    _In_ PFLT_PORT ClientPort,
    _In_opt_ PVOID ServerPortCookie,
    _In_reads_bytes_opt_(SizeOfContext) PVOID ConnectionContext,
    _In_ ULONG SizeOfContext,
    _Outptr_result_maybenull_ PVOID *ConnectionCookie)
{
    UNREFERENCED_PARAMETER(ServerPortCookie);
    UNREFERENCED_PARAMETER(ConnectionContext);
    UNREFERENCED_PARAMETER(SizeOfContext);
    *ConnectionCookie = NULL;
    gClientPort = ClientPort;
    return STATUS_SUCCESS;
}

static VOID AgentFsDisconnect(_In_opt_ PVOID ConnectionCookie)
{
    UNREFERENCED_PARAMETER(ConnectionCookie);
    FltCloseClientPort(gFilter, &gClientPort);
}

static NTSTATUS AgentFsMessage(
    _In_opt_ PVOID PortCookie,
    _In_reads_bytes_opt_(InputBufferLength) PVOID InputBuffer,
    _In_ ULONG InputBufferLength,
    _Out_writes_bytes_to_opt_(OutputBufferLength, *ReturnOutputBufferLength) PVOID OutputBuffer,
    _In_ ULONG OutputBufferLength,
    _Out_ PULONG ReturnOutputBufferLength)
{
    UNREFERENCED_PARAMETER(PortCookie);
    if (InputBufferLength < sizeof(AGENTFS_REQUEST) || OutputBufferLength < sizeof(AGENTFS_REPLY)) {
        return STATUS_INVALID_PARAMETER;
    }
    PAGENTFS_REQUEST request = (PAGENTFS_REQUEST)InputBuffer;
    PAGENTFS_REPLY reply = (PAGENTFS_REPLY)OutputBuffer;
    NTSTATUS status = STATUS_SUCCESS;
    if (request->Version != AGENTFS_IOCTL_VERSION) {
        status = STATUS_REVISION_MISMATCH;
    } else if (request->Kind == AgentFsRegisterProcess) {
        status = AgentFsRegister(request);
    } else if (request->Kind == AgentFsUnregisterProcess) {
        AgentFsUnregister(request->ProcessId);
    } else if (request->Kind != AgentFsCheck) {
        status = STATUS_INVALID_PARAMETER;
    }
    AgentFsWriteReply(reply, NT_SUCCESS(status) ? AGENTFS_REPLY_OK : AGENTFS_REPLY_ERROR, status);
    *ReturnOutputBufferLength = sizeof(AGENTFS_REPLY);
    return STATUS_SUCCESS;
}

static NTSTATUS AgentFsCreatePort(_In_ PDRIVER_OBJECT DriverObject)
{
    UNREFERENCED_PARAMETER(DriverObject);
    UNICODE_STRING portName;
    OBJECT_ATTRIBUTES oa;
    RtlInitUnicodeString(&portName, AGENTFS_PORT_NAME);
    InitializeObjectAttributes(&oa, &portName, OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL, NULL);
    return FltCreateCommunicationPort(
        gFilter,
        &gServerPort,
        &oa,
        NULL,
        AgentFsConnect,
        AgentFsDisconnect,
        AgentFsMessage,
        1);
}

static NTSTATUS AgentFsUnload(_In_ FLT_FILTER_UNLOAD_FLAGS Flags)
{
    UNREFERENCED_PARAMETER(Flags);
    if (gServerPort != NULL) {
        FltCloseCommunicationPort(gServerPort);
    }
    (VOID)PsSetCreateProcessNotifyRoutineEx(AgentFsProcessNotify, TRUE);
    ExAcquireFastMutex(&gEnvLock);
    while (!IsListEmpty(&gEnvs)) {
        PLIST_ENTRY link = RemoveHeadList(&gEnvs);
        PAGENTFS_ENV env = CONTAINING_RECORD(link, AGENTFS_ENV, Link);
        ExReleaseFastMutex(&gEnvLock);
        AgentFsFreeEnv(env);
        ExAcquireFastMutex(&gEnvLock);
    }
    ExReleaseFastMutex(&gEnvLock);
    ExAcquireFastMutex(&gDirStateLock);
    while (!IsListEmpty(&gDirStates)) {
        PLIST_ENTRY link = RemoveHeadList(&gDirStates);
        PAGENTFS_DIR_STATE state = CONTAINING_RECORD(link, AGENTFS_DIR_STATE, Link);
        ExReleaseFastMutex(&gDirStateLock);
        ExFreePoolWithTag(state, AGENTFS_TAG);
        ExAcquireFastMutex(&gDirStateLock);
    }
    ExReleaseFastMutex(&gDirStateLock);
    if (gFilter != NULL) {
        FltUnregisterFilter(gFilter);
    }
    return STATUS_SUCCESS;
}

NTSTATUS DriverEntry(_In_ PDRIVER_OBJECT DriverObject, _In_ PUNICODE_STRING RegistryPath)
{
    UNREFERENCED_PARAMETER(RegistryPath);
    ExInitializeFastMutex(&gEnvLock);
    InitializeListHead(&gEnvs);
    ExInitializeFastMutex(&gDirStateLock);
    InitializeListHead(&gDirStates);
    NTSTATUS status = FltRegisterFilter(DriverObject, &FilterRegistration, &gFilter);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    status = AgentFsCreatePort(DriverObject);
    if (!NT_SUCCESS(status)) {
        FltUnregisterFilter(gFilter);
        return status;
    }
    status = PsSetCreateProcessNotifyRoutineEx(AgentFsProcessNotify, FALSE);
    if (!NT_SUCCESS(status)) {
        FltCloseCommunicationPort(gServerPort);
        FltUnregisterFilter(gFilter);
        return status;
    }
    status = FltStartFiltering(gFilter);
    if (!NT_SUCCESS(status)) {
        (VOID)PsSetCreateProcessNotifyRoutineEx(AgentFsProcessNotify, TRUE);
        FltCloseCommunicationPort(gServerPort);
        FltUnregisterFilter(gFilter);
    }
    return status;
}
