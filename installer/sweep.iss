; Sweep 安装程序（Inno Setup 6.5 及以上，也兼容 7）。
;
; 本地构建：
;   cargo build --release
;   ISCC.exe /DAppVersion=0.3.0 /DIconFile=<target\release\build\sweep-*\out\sweep.ico> installer\sweep.iss
; CI（.github/workflows/release.yml）会传入版本号、图标与输出文件名。
;
; 自动更新时 Sweep 以 /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS /LAUNCH=1 运行它，
; /LAUNCH=1 让静默安装结束后重新启动 Sweep（普通的静默部署不会启动）。

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
; 文件版本只能是数字（预发布标签如 0.4.0-rc.1 要去掉后缀）。
#ifndef FileVersion
  #define FileVersion AppVersion
#endif
#ifndef SourceExe
  #define SourceExe "..\target\release\sweep.exe"
#endif
#ifndef IconFile
  #define IconFile "sweep.ico"
#endif
#ifndef OutputDir
  #define OutputDir "..\dist"
#endif
#ifndef OutputName
  #define OutputName "sweep-v" + AppVersion + "-windows-x64-setup"
#endif

[Setup]
; AppId 决定卸载信息的位置（Sweep 的更新检查也靠它判断是否为安装版），不要修改。
AppId={{9B83B075-7D2D-49D9-8547-11D6640C0858}
AppName=Sweep
AppVersion={#AppVersion}
AppVerName=Sweep {#AppVersion}
AppPublisher=Jannchie
AppPublisherURL=https://github.com/Jannchie/sweep
AppSupportURL=https://github.com/Jannchie/sweep/issues
AppUpdatesURL=https://github.com/Jannchie/sweep/releases
VersionInfoVersion={#FileVersion}
DefaultDirName={autopf}\Sweep
DisableProgramGroupPage=yes
DisableWelcomePage=yes
; 默认为所有用户安装到 Program Files；/CURRENTUSER 可改为当前用户安装（不需要管理员权限）。
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=commandline
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutputDir}
OutputBaseFilename={#OutputName}
SetupIconFile={#IconFile}
UninstallDisplayIcon={app}\sweep.exe
UninstallDisplayName=Sweep
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; 用 Restart Manager 关闭正在运行的 Sweep，才能替换 sweep.exe。
CloseApplications=yes
RestartApplications=no
ShowLanguageDialog=auto
LanguageDetectionMethod=uilanguage

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "zh"; MessagesFile: "ChineseSimplified.isl"
Name: "ja"; MessagesFile: "compiler:Languages\Japanese.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "sweep.exe"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Sweep"; Filename: "{app}\sweep.exe"
Name: "{autodesktop}\Sweep"; Filename: "{app}\sweep.exe"; Tasks: desktopicon

[Registry]
; App Paths：Win+R 或 ShellExecute 可以直接用 sweep 启动。HKA 在所有用户安装时是 HKLM，当前用户安装时是 HKCU。
Root: HKA; Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\sweep.exe"; ValueType: string; ValueName: ""; ValueData: "{app}\sweep.exe"; Flags: uninsdeletekey
Root: HKA; Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\sweep.exe"; ValueType: string; ValueName: "Path"; ValueData: "{app}"

[Run]
Filename: "{app}\sweep.exe"; Description: "{cm:LaunchProgram,Sweep}"; Flags: nowait postinstall skipifsilent runasoriginaluser
Filename: "{app}\sweep.exe"; Flags: nowait runasoriginaluser; Check: RelaunchAfterUpdate

[Code]
{ 自动更新传入 /LAUNCH=1：静默安装完成后重新启动 Sweep。 }
function RelaunchAfterUpdate: Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:LAUNCH|0}') = '1');
end;
