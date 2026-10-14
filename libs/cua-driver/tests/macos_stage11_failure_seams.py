#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Generate included-source host-free failure regressions; no GUI or native claims."""
import argparse
from pathlib import Path
p = argparse.ArgumentParser()
p.add_argument('output', type=Path)
p.add_argument('--blocked-index', action='store_true', help='Extract exact blocked index rather than working source')
a = p.parse_args()
out = a.output.resolve()
repo = Path(__file__).resolve().parents[3]
if out.is_relative_to(repo):
    p.error('Generate fixtures outside the source checkout')
root = repo / 'libs/cua-driver/rust/crates/platform-macos/src'
def block(s, needle):
    start = s.index(needle); pos = s.index('{', start); depth = 1; end = pos + 1
    while depth:
        depth += (s[end] == '{') - (s[end] == '}'); end += 1
    return s[start:end]
import subprocess
import re
def read(name):
    path = root / name
    if a.blocked_index:
        return subprocess.check_output(['git', 'show', ':' + str(path.relative_to(repo))], cwd=repo, text=True)
    return path.read_text()
ae = read('apple_events.rs')
sv = read('tools/set_value.rs')
out.mkdir(parents=True, exist_ok=True)
guard = out / 'desktop_pointer_guard.rs'
guard.write_text(read('tools/desktop_pointer_guard.rs'))
extra_field = 'mutation_possible: true,' if 'pub mutation_possible: bool' in ae else ''
extra_arg = ', true' if 'mutation_possible: bool' in block(sv, 'fn finder_rename_result(') else ''
source = r'''#![allow(dead_code)]
use cua_driver_core::protocol::ToolResult;
mod apps {pub fn frontmost_pid()->Option<i32>{Some(1)} pub fn bundle_id_for_pid(_:i32)->Option<String>{Some("com.example.other".into())}}
mod windows {
 #[derive(Clone)] pub struct Bounds {pub x:f64,pub y:f64,pub width:f64,pub height:f64}
 #[derive(Clone)] pub struct Window {pub pid:i32,pub window_id:u32,pub app_name:String,pub bounds:Bounds,pub layer:i32,pub is_on_screen:bool,pub z_index:usize}
 static MODE:std::sync::atomic::AtomicUsize=std::sync::atomic::AtomicUsize::new(0);
 pub fn mode(n:usize){MODE.store(n,std::sync::atomic::Ordering::SeqCst)}
 pub fn visible_windows_checked()->anyhow::Result<Vec<Window>> {match MODE.load(std::sync::atomic::Ordering::SeqCst){0=>Err(anyhow::anyhow!("CGWindowList returned null")),1=>Ok(vec![]),2=>Ok(vec![Window{pid:2,window_id:42,app_name:"Other".into(),bounds:Bounds{x:0.,y:0.,width:10.,height:10.},layer:0,is_on_screen:true,z_index:1}]),3=>Err(anyhow::anyhow!("invalid enumeration")),_=>panic!("native task failed")}}
 pub fn visible_windows()->Vec<Window>{visible_windows_checked().unwrap_or_default()}
}
mod tools {
 pub mod type_text {pub fn is_chromium_identity(id: &str)->bool{id=="com.google.Chrome"}}
 #[path="GUARD"] pub mod desktop_pointer_guard;
}
mod apple_events {
 STRUCT
}
const FILE_NAME_NEEDS_RENAME:&str="file_name_needs_rename";
RECORD
RESULT
#[tokio::test] async fn pointer_observation_contract(){
 for tool in ["click","drag","scroll","move_cursor"] {
  for mode in [0,3,4] {windows::mode(mode);let r=tools::desktop_pointer_guard::check(tool,&serde_json::json!({}),vec![(1.,1.)]).await.expect("unknown observation must refuse");assert_eq!(r.structured_content.unwrap()["code"],"background_unavailable");}
  windows::mode(1);assert!(tools::desktop_pointer_guard::check(tool,&serde_json::json!({}),vec![(1.,1.)]).await.is_none(),"truly empty is allowed");
  windows::mode(2);let r=tools::desktop_pointer_guard::check(tool,&serde_json::json!({}),vec![(1.,1.)]).await.unwrap();assert_eq!(r.structured_content.unwrap()["code"],"background_unavailable");
  windows::mode(0);assert!(tools::desktop_pointer_guard::check(tool,&serde_json::json!({"delivery_mode":"foreground"}),vec![(1.,1.)]).await.is_none());
 }
}
#[test] fn finder_post_send_error_is_uncertain(){
 for code in ["automation_cancelled","rename_unconfirmed","automation_timeout","automation_failed","automation_denied"] {
  let r=finder_rename_result(7,42,std::path::Path::new("/evidence/a.txt"),"b.txt",Ok(Err(apple_events::ScriptRefusal{code,reason:"after send".into(),FIELD}))ARG);
  assert_eq!(r.structured_content.as_ref().unwrap()["effect"],"unverifiable", "{code}");
  assert_eq!(r.structured_content.as_ref().unwrap()["cause"],code);
  let text=serde_json::to_value(r).unwrap()["content"][0]["text"].as_str().unwrap().to_owned();assert!(!text.contains("keeps its name"));assert!(!text.contains("nothing else was sent"));assert!(text.contains("fresh"));
 }
}
'''.replace('GUARD', str(guard)).replace('STRUCT', '#[derive(Debug,Clone,PartialEq,Eq)]\n' + block(ae,'pub struct ScriptRefusal {')).replace('RECORD',block(sv,'fn rename_record(')).replace('RESULT',block(sv,'fn finder_rename_result(')).replace('FIELD',extra_field).replace('ARG',extra_arg)
task_error = 'tokio::spawn(async { panic!("fixture task panic") }).await.unwrap_err()' if 'tokio::task::JoinError' in sv else 'cua_driver_core::blocking::NativeDeadlineError::Panicked'
task_flag = ', flag' if extra_arg else ''
source += r'''
#[tokio::test] async fn finder_task_failure_retains_mutation_uncertainty(){
 for flag in [false,true]{let err=TASK_ERROR;let r=finder_rename_result(7,42,std::path::Path::new("/evidence/a.txt"),"b.txt",Err(err)FLAG);let detail=r.structured_content.unwrap();assert_eq!(detail["effect"],if flag{"unverifiable"}else{"refused"});assert_eq!(detail["cause"],"automation_task_failed");}
}
'''.replace('TASK_ERROR',task_error).replace('FLAG',task_flag)
# Include the production failure presenter when the fixed caller uses it.
if 'pub(crate) fn script_failure_result(' in ae:
    helpers = block(ae, 'pub(crate) fn script_failure_result(')
    source = source.replace('mod apple_events {', 'mod apple_events {\n' + helpers)
(out / 'src').mkdir(parents=True, exist_ok=True)
(out / 'src/lib.rs').write_text(source)
(out / 'Cargo.toml').write_text(f'''[package]
name="cua-stage11-failure-seams"
version="0.0.0"
edition="2021"
[dependencies]
anyhow="1"
serde_json="1"
tokio={{version="1",features=["macros","rt"]}}
cua-driver-core={{path="{repo}/libs/cua-driver/rust/crates/cua-driver-core"}}
''')
# Compile actual scripting control flow against a fake subprocess seam. These
# are simulated outcomes, never output from a native macOS run.
mutation_type = "#[derive(Clone,Default)] struct ScriptMutation; impl ScriptMutation {fn possible(&self)->bool{false}}"
# The tuple struct has no brace; extract its single declaration explicitly.
if 'pub(crate) struct ScriptMutation(' in ae:
    mutation_type = '#[derive(Clone,Default)]\n' + re.search(r'pub\(crate\) struct ScriptMutation[^;]+;', ae).group(0) + '\n' + block(ae, 'impl ScriptMutation {')
functions = ['pub fn run_script(', 'fn script_error_refusal(', 'fn folder_lists(']
if 'fn run_script_tracked(' in ae:
    functions.append('fn run_script_tracked(')
functions += ['pub(crate) fn finder_rename_tracked(' if 'pub(crate) fn finder_rename_tracked(' in ae else 'pub fn finder_rename(', 'pub(crate) fn save_document_tracked(' if 'pub(crate) fn save_document_tracked(' in ae else 'pub fn save_document(', 'pub fn valid_file_name(']
func_source = '\n'.join(block(ae, n) for n in functions if n != 'fn folder_lists(')
# Readback has a fake observer; production retry/cancellation logic is included.
consts = '\n'.join(re.search(r'const ' + n + r': &str = r#".*?"#;', ae, re.S).group(0) for n in ['FINDER_RENAME_SCRIPT', 'SAVE_DOCUMENT_SCRIPT'])
possible = 'error.mutation_possible' if extra_field else 'false'
false_field = 'mutation_possible: false,' if extra_field else ''
tracker_arg = ', &mutation' if extra_field else ''
runner = 'run_script_tracked("script", &[], Duration::ZERO, Some(&mutation))' if extra_field else 'run_script("script", &[], Duration::ZERO)'
native = r'''
mod script_execution {
 use std::time::{Duration,Instant}; use std::path::Path; use std::process::Stdio; use std::io::Write; use std::os::unix::process::ExitStatusExt;
 use crate::apple_events::ScriptRefusal;
 MUTATION
 #[derive(Default)] struct Scenario {mode:usize,sends:usize,applied:usize,output:String,permission_denied:bool,spawned:bool}
 thread_local! {static WORLD:std::cell::RefCell<Scenario>=std::cell::RefCell::new(Scenario::default());}
 fn setup(mode:usize,output:&str){WORLD.with(|w|*w.borrow_mut()=Scenario{mode,output:output.into(),..Scenario::default()});}
 fn check_cancelled()->Result<(),ScriptRefusal>{if WORLD.with(|w|{let w=w.borrow();w.mode==7 || (w.mode==9 && w.spawned) || (w.mode==8 && w.sends>0)}){Err(ScriptRefusal{FIELD code:"automation_cancelled",reason:"fixture cancellation".into()})}else{Ok(())}}
 fn ensure_permitted(_: &str,_:&str)->Result<(),ScriptRefusal>{if WORLD.with(|w|w.borrow().permission_denied){Err(ScriptRefusal{FIELD code:"automation_denied",reason:"fixture pre-send denial".into()})}else{Ok(())}}
 fn folder_lists(_: &Path,_:&str)->bool{false}
 fn modified_time(_: &Path)->Option<std::time::SystemTime>{None}
 fn wait_for_new_mtime(_: &Path,_:Option<std::time::SystemTime>,_:Duration)->bool{false}
 const FINDER_BUNDLE_ID:&str="com.apple.finder";
 #[derive(Debug)] struct SavedDocument{file_written:bool}
 struct Command; impl Command {fn new(_: &str)->Self{Self} fn arg(self,_:&str)->Self{self} fn args(self,_:&[&str])->Self{self} fn stdin(self,_:Stdio)->Self{self} fn stdout(self,_:Stdio)->Self{self} fn stderr(self,_:Stdio)->Self{self} fn spawn(self)->std::io::Result<Child>{WORLD.with(|w|w.borrow_mut().spawned=true);if WORLD.with(|w|w.borrow().mode==1){Err(std::io::Error::other("fixture spawn failed"))}else{Ok(Child{stdin:Some(Input)})}}}
 struct Input; impl Write for Input{fn write(&mut self,b:&[u8])->std::io::Result<usize>{WORLD.with(|w|{let mut w=w.borrow_mut();w.sends+=1;w.applied+=1;});if WORLD.with(|w|w.borrow().mode==10){Err(std::io::Error::other("fixture partial script write"))}else{Ok(b.len())}} fn flush(&mut self)->std::io::Result<()>{Ok(())}}
 struct Child{stdin:Option<Input>} impl Child {
 fn try_wait(&mut self)->std::io::Result<Option<std::process::ExitStatus>>{match WORLD.with(|w|w.borrow().mode){3=>Ok(None),4=>Err(std::io::Error::other("fixture native wait error")),_=>Ok(Some(std::process::ExitStatus::from_raw(0)))}}
 fn kill(&mut self)->std::io::Result<()>{Ok(())} fn wait(&mut self)->std::io::Result<std::process::ExitStatus>{Ok(std::process::ExitStatus::from_raw(0))}
 fn wait_with_output(self)->std::io::Result<std::process::Output>{WORLD.with(|w|{let w=w.borrow();if w.mode==5{return Err(std::io::Error::other("fixture output failed"))};let failed=w.mode==2||w.mode==6;Ok(std::process::Output{status:std::process::ExitStatus::from_raw(if failed{256}else{0}),stdout:w.output.as_bytes().to_vec(),stderr:if w.mode==6{b"execution error: fixture denied (-1743)".to_vec()}else{b"execution error: fixture failure (-48)".to_vec()}})})}}
 CONSTS
 FUNCTIONS
 #[test] fn execution_errors_after_application_stay_uncertain(){
  for (mode,code) in [(2,"automation_failed"),(3,"automation_timeout"),(4,"automation_failed"),(5,"automation_failed"),(6,"automation_denied"),(10,"automation_failed")] {
   setup(mode,"ok");let mutation=ScriptMutation::default();let error=RUNNER.unwrap_err();assert_eq!(error.code,code);assert!(POSSIBLE,"post-send {code}");assert!(mutation.possible(),"armed before execution");WORLD.with(|w|{let w=w.borrow();assert_eq!(w.sends,1);assert_eq!(w.applied,1)});
  }
 }
 #[test] fn pre_send_errors_are_refused(){for (mode,code) in [(1,"automation_failed"),(7,"automation_cancelled"),(9,"automation_cancelled")] {setup(mode,"");let mutation=ScriptMutation::default();let error=RUNNER.unwrap_err();assert_eq!(error.code,code);assert!(!POSSIBLE);WORLD.with(|w|assert_eq!(w.borrow().sends,0));}}
 #[test] fn finder_cancelled_readback_is_uncertain(){setup(8,"ok");let mutation=ScriptMutation::default();let error=finder_rename(Path::new("/unobserved-stage11/a.txt"),"b.txt"TRACKER).unwrap_err();assert_eq!(error.code,"automation_cancelled");assert!(POSSIBLE);assert!(mutation.possible());WORLD.with(|w|assert_eq!(w.borrow().sends,1));}
 #[test] fn finder_unconfirmed_readback_is_uncertain(){setup(0,"ok");let mutation=ScriptMutation::default();let error=finder_rename(Path::new("/unobserved-stage11/a.txt"),"b.txt"TRACKER).unwrap_err();assert_eq!(error.code,"rename_unconfirmed");assert!(POSSIBLE);WORLD.with(|w|assert_eq!(w.borrow().sends,1));}
 #[test] fn save_readback_errors_are_uncertain(){for response in ["true","unexpected"]{setup(0,response);let mutation=ScriptMutation::default();let error=save_document("com.apple.TextEdit","TextEdit",Path::new("/unobserved-stage11/a.txt")TRACKER).unwrap_err();assert_eq!(error.code,"save_unconfirmed");assert!(POSSIBLE);assert!(mutation.possible());}}
 #[test] fn save_permission_denied_before_execution(){setup(0,"");WORLD.with(|w|w.borrow_mut().permission_denied=true);let mutation=ScriptMutation::default();let error=save_document("com.apple.TextEdit","TextEdit",Path::new("/unobserved-stage11/a.txt")TRACKER).unwrap_err();assert_eq!(error.code,"automation_denied");assert!(!POSSIBLE);WORLD.with(|w|assert_eq!(w.borrow().sends,0));}
}
'''.replace('MUTATION',mutation_type).replace('FIELD',false_field).replace('CONSTS',consts).replace('FUNCTIONS',func_source).replace('RUNNER',runner).replace('POSSIBLE',possible).replace('TRACKER',tracker_arg)
if extra_field:
    native = native.replace('let error=finder_rename(', 'let error=finder_rename_tracked(').replace('let error=save_document(', 'let error=save_document_tracked(')
with (out / 'src/lib.rs').open('a') as f:
    f.write(native)
# Static linkage is separate from dynamic fixtures: the actual native null and
# parse branches must preserve errors and the guarded callers must use them.
import unittest
windows_source = read('windows.rs')
if 'fn decode_onscreen_property(' in windows_source:
    with (out / 'src/lib.rs').open('a') as f:
        f.write('\n#[allow(non_upper_case_globals)] const kCGWindowListOptionOnScreenOnly: u32 = 1;\n')
        f.write('#[allow(non_upper_case_globals)] const kCGWindowListExcludeDesktopElements: u32 = 16;\n')
        f.write(block(windows_source, 'fn decode_onscreen_property('))
        f.write('\n#[test]\n' + block(windows_source, 'fn optional_onscreen_property_preserves_valid_records('))

class NativeLinkage(unittest.TestCase):
    def test_optional_onscreen_key_preserves_valid_records(self):
        enumeration = block(windows_source, 'fn enumerate_windows(')
        getter = block(enumeration, 'let get_bool =')
        self.assertIn('Option<Option<bool>>', getter)
        self.assertIn('.map(', getter)
        self.assertIn('decode_onscreen_property(get_bool("kCGWindowIsOnscreen"), options)?', enumeration)
        self.assertIn('None => Ok(options & kCGWindowListOptionOnScreenOnly != 0)', windows_source)

    def test_null_and_parse_preserve_failure(self):
        w = read('windows.rs')
        enumeration = block(w, 'fn enumerate_windows(')
        null = block(enumeration, 'if raw_ref.is_null()')
        self.assertIn('Err(', null)
        self.assertIn('bail!("CGWindowList entry {idx} is not a dictionary")', enumeration)
        self.assertIn('invalid bounds', enumeration)
        self.assertIn('invalid {key}', enumeration)
        self.assertNotIn('.unwrap_or(0)', enumeration)
        self.assertNotIn('.unwrap_or(false)', enumeration)
        self.assertIn('visible_windows_checked()', guard.read_text())
    def test_save_errors_do_not_claim_unchanged(self):
        m = read('tools/invoke_menu.rs')
        self.assertNotIn('Nothing was saved', m)
        self.assertNotIn('because it already held the', m)
        self.assertIn('script_failure_result(', m)
    def test_mutation_armed_before_execution_and_consumers_track_task_errors(self):
        run = block(ae, 'fn run_script_tracked(') if 'fn run_script_tracked(' in ae else block(ae, 'pub fn run_script(')
        self.assertIn('mutation.arm()', run)
        self.assertLess(run.index('mutation.arm()'), run.index('Command::new('))
        self.assertIn('mutation.possible()', sv)
        self.assertIn('mutation.possible()', read('tools/invoke_menu.rs'))
        for tool in ['click', 'drag', 'scroll', 'move_cursor']:
            caller = read('tools/' + tool + '.rs')
            self.assertIn('desktop_pointer_guard::check(', caller)
        self.assertIn('code: "rename_unconfirmed"', ae)
        self.assertNotIn('run_script(', block(sv, 'fn finder_rename_result('))
if __name__ == '__main__':
    unittest.main(argv=['native-linkage'])
