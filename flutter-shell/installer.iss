; InDesk 主控端安装器（Flutter 壳 release + indesk-core.exe）
; Flutter 产物可压缩性好，Inno lzma2 满压直接打（无需 7z 外挂载荷）。
#define MyAppName "InDesk"
#define MyAppVersion "2.0.0"
#define MyAppPublisher "yomm"
#define MyAppExeName "indesk.exe"

[Setup]
AppId={{A3D91C77-52E4-4B08-9F2C-7E6B1D8A9013}
AppName={#MyAppName} 主控端
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\InDesk-Controller
DefaultGroupName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}
OutputDir=inno
OutputBaseFilename=InDesk-Controller-2.0.0-x64-setup
SetupIconFile=G:\InDesk\electron-shell\icon.ico
Compression=lzma2/max
SolidCompression=yes
LZMAUseSeparateProcess=yes
LZMADictionarySize=65536
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
WizardStyle=modern
DisableProgramGroupPage=yes
CloseApplications=yes

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "附加任务："

[Files]
Source: "G:\InDesk\flutter-shell\build\windows\x64\runner\Release\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "启动 {#MyAppName}"; Flags: nowait postinstall skipifsilent
