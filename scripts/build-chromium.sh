#!/usr/bin/env bash
# Build a relocatable browser runtime. CEF downloads its pinned distribution at
# build time; the installed app never fetches executable code on first launch.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
output="${1:?usage: build-chromium.sh OUTPUT_DIRECTORY [debug|release]}"
profile="${2:-release}"
case "$profile" in debug) flags="";; release) flags="--release";; *) echo 'Profile must be debug or release' >&2; exit 1;; esac
mkdir -p "$output"
output="$(cd "$output" && pwd)"
cd "$root"
cargo build --locked -p noches-chromium --features runtime ${flags:+$flags} --message-format=json > "$output/build.jsonl"
python3 - "$output" "$profile" <<'PY'
import json,os,pathlib,shutil,sys,plistlib
out=pathlib.Path(sys.argv[1]); records=[json.loads(l) for l in (out/'build.jsonl').read_text().splitlines() if l.startswith('{')]
exe=next(pathlib.Path(r['executable']) for r in records if r.get('reason')=='compiler-artifact' and r.get('executable') and r['target']['name']=='noches-chromium')
build=next(pathlib.Path(r['out_dir']) for r in records if r.get('reason')=='build-script-executed' and 'cef-dll-sys' in r['package_id'])
cef=pathlib.Path(os.environ['CEF_PATH']) if 'CEF_PATH' in os.environ else next(p for p in build.glob('cef_*') if p.is_dir())
if sys.platform=='darwin':
 app=out/'Noches Browser.app';contents=app/'Contents';frameworks=contents/'Frameworks';binary=contents/'MacOS';binary.mkdir(parents=True,exist_ok=True);frameworks.mkdir(parents=True,exist_ok=True);(contents/'Resources').mkdir(exist_ok=True)
 shutil.copy2(exe,binary/'noches-chromium')
 shutil.copy2(cef/'CREDITS.html',contents/'Resources'/'Chromium-CREDITS.html')
 shutil.copy2(pathlib.Path('dist/browser/CEF-LICENSE.txt'),contents/'Resources'/'CEF-LICENSE.txt')
 shutil.copy2(pathlib.Path('dist/browser/THIRD_PARTY_NOTICES.md'),contents/'Resources'/'THIRD_PARTY_NOTICES.md')
 shutil.copytree(cef/'Chromium Embedded Framework.framework',frameworks/'Chromium Embedded Framework.framework',symlinks=True,dirs_exist_ok=True)
 def plist(path,name,executable,identifier):
  path.write_bytes(plistlib.dumps(dict(CFBundleName=name,CFBundleDisplayName=name,CFBundleExecutable=executable,CFBundleIdentifier=identifier,CFBundlePackageType='APPL',CFBundleVersion='1',LSUIElement=True,NSHighResolutionCapable=True)))
 plist(contents/'Info.plist','Noches Browser','noches-chromium','app.noches.browser')
 for suffix in ['', ' (Alerts)', ' (GPU)', ' (Plugin)', ' (Renderer)']:
  name='Noches Browser Helper'+suffix;sub=frameworks/(name+'.app')/'Contents';(sub/'MacOS').mkdir(parents=True,exist_ok=True);(sub/'Resources').mkdir(exist_ok=True)
  shutil.copy2(exe,sub/'MacOS'/name);plist(sub/'Info.plist',name,name,'app.noches.browser.helper'+suffix.replace(' ','').replace('(','').replace(')','').lower())
 print(binary/'noches-chromium')
else:
 dest=out/'browser';dest.mkdir(parents=True,exist_ok=True)
 for source in cef.iterdir():
  if source.name in ['include','cmake','libcef_dll','CMakeLists.txt','archive.json']: continue
  target=dest/source.name
  if source.is_dir(): shutil.copytree(source,target,symlinks=True,dirs_exist_ok=True)
  else: shutil.copy2(source,target)
 shutil.copy2(pathlib.Path('dist/browser/THIRD_PARTY_NOTICES.md'),dest/'THIRD_PARTY_NOTICES.md');shutil.copy2(exe,dest/'noches-chromium');shutil.copy2(pathlib.Path('dist/browser/CEF-LICENSE.txt'),dest/'CEF-LICENSE.txt');print(dest/'noches-chromium')
PY
rm -f "$output/build.jsonl"
