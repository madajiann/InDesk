; InDesk 统一安装包 —— 一个包，双角色
; 主控 indesk.exe（Flutter 壳 + indesk-core.exe）+ 被控核 indesk-desktop.exe（headless --core）
;
; 路径全部相对本文件所在目录（installer/），仓库 clone 下来即可直接编译，
; 不依赖任何绝对路径。编译前先完成 README「构建」一节的三步产物。
;
; 编译：ISCC.exe installer.iss
; 产物：out\InDesk-<版本>-x64-setup.exe
#define MyAppName "InDesk"
#define MyAppVersion "2.3.49"
#define MyAppPublisher "yomm"
#define MyAppExeName "indesk.exe"
#define MyAppHostExeName "indesk-desktop.exe"

[Setup]
AppId={{7C2E4A19-8B3D-4F6E-9A51-D04C7B2E1308}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\InDesk
DefaultGroupName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}
OutputDir=..\out
OutputBaseFilename=InDesk-{#MyAppVersion}-x64-setup
SetupIconFile=..\brand\indesk.ico
Compression=lzma2/max
SolidCompression=yes
LZMAUseSeparateProcess=yes
LZMADictionarySize=65536
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
WizardStyle=modern
DisableProgramGroupPage=yes
CloseApplications=force
RestartApplications=no
; Inno 6.3+ 默认 auto：机器上有旧安装记录就跳过目录选择页、直接沿用旧盘符。
; 这里强制显示，让用户每次都能自选安装位置。
DisableDirPage=no

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "附加任务："
Name: "autostart"; Description: "开机自动启动（保证随时可被远程连接）"; GroupDescription: "附加任务："; Flags: unchecked

[Files]
; 主控（Flutter 壳 + core + 资源）；排除本机测试身份文件与日志
Source: "..\flutter-shell\build\windows\x64\runner\Release\*"; DestDir: "{app}"; Excludes: "controller_identity.json,host_identity.json,*.log,*_diag.log,flutter_assets_broken"; Flags: ignoreversion recursesubdirs createallsubdirs
; 被控核（headless --core 模式，零 WebView2 依赖）
Source: "..\web\src-tauri\target\release\indesk-desktop.exe"; DestDir: "{app}"; Flags: ignoreversion
; app-local VC++ 运行库。Flutter Windows 产物是 /MD 动态链接，客户机没装
; VC++ 2015-2022 运行库时会弹「计算机中丢失 VCRUNTIME140_1.dll」。
; 仓库不包含微软的运行库二进制（按微软再分发条款获取），请自己放到
; installer\redist\ 下，或改为在目标机安装 VC++ 2015-2022 x64 运行库。
Source: "redist\msvcp140.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "redist\vcruntime140.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "redist\vcruntime140_1.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon
Name: "{commonstartup}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: autostart

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "启动 {#MyAppName}"; Flags: nowait postinstall skipifsilent
; 自更新路径：壳内的更新器以 `/UPDATED=1 /SILENT` 调起本安装器，
; 静默模式没有"完成"页，装完必须由这里把程序拉起来，
; 否则用户看到的是"更新完之后程序就不见了"。
Filename: "{app}\{#MyAppExeName}"; Flags: nowait; Check: IsUpdateInstall

[UninstallRun]
Filename: "taskkill"; Parameters: "/f /im indesk.exe"; Flags: runhidden; RunOnceId: "KillCtl"
Filename: "taskkill"; Parameters: "/f /im indesk-desktop.exe"; Flags: runhidden; RunOnceId: "KillHost"
Filename: "taskkill"; Parameters: "/f /im indesk-core.exe"; Flags: runhidden; RunOnceId: "KillCore"

[UninstallDelete]
Type: filesandordirs; Name: "{localappdata}\com.indesk.app"

[Code]
// 自更新标记：桌面端更新器用 /UPDATED=1 调起安装器。
// GetCmdTail 返回的是不含 exe 路径的完整命令行。
function IsUpdateInstall(): Boolean;
begin
  Result := Pos('/UPDATED', Uppercase(GetCmdTail)) > 0;
end;

// PrepareToInstall: 复制文件前先杀掉全部 InDesk 进程，让向导永远不出现
// "文件正在使用"那一页。RestartManager 的强制关闭只是兜底——托盘里的
// indesk.exe 正常关闭是关不掉的，必须 taskkill。
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  R: Integer;
begin
  Result := '';
  Exec('taskkill.exe', '/f /im indesk.exe', '', 0, ewWaitUntilTerminated, R);
  Exec('taskkill.exe', '/f /im indesk-desktop.exe', '', 0, ewWaitUntilTerminated, R);
  Exec('taskkill.exe', '/f /im indesk-core.exe', '', 0, ewWaitUntilTerminated, R);
  Sleep(1200); // 等操作系统释放文件句柄
end;
