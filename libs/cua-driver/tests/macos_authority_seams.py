#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# Copyright (c) 2026 Cua AI, Inc.
"""Host-free regression harness: extracts production Rust and supplies fake APIs.

Generate into explicit scratch storage, then test through the caller's guarded
environment. A standalone `cargo test --locked --offline` needs a compatible
lockfile; a copied workspace lock alone may require changes. Without re-resolving,
build cua-driver-core with locked offline workspace Cargo and compile src/lib.rs
with `rustc --test` against its emitted cua_driver_core, anyhow, and image rlibs.
This tests control flow and real image encoding on Linux; it neither accesses a
desktop nor qualifies macOS behavior.
"""
from pathlib import Path
import argparse
parser = argparse.ArgumentParser(description="Generate host-free Rust tests from production macOS seams; no native qualification.")
parser.add_argument("output", type=Path, help="Existing scratch directory (not /tmp)")
args = parser.parse_args()
out = args.output.resolve()
if str(out).startswith("/tmp/") or out == Path("/tmp"):
    parser.error("Use scratch/state storage, not /tmp")
repo = Path(__file__).resolve().parents[3]
root = repo / "libs/cua-driver/rust/crates/platform-macos/src"
def block(s, needle):
 a=s.index(needle); b=s.index('{',a); d=1; e=b+1
 while d:
  d+=(s[e]=='{')-(s[e]=='}');e+=1
 return s[a:e]
menu_source=(root/'tools/invoke_menu.rs').read_text()
menu=block(menu_source,'unsafe fn invoke_path_in_background(')
doc=block((root/'document_edit.rs').read_text(),'pub unsafe fn ensure_marked_after_ax_write(')
cap=(root/'capture.rs').read_text()
route=block(cap,'fn capture_window_with_space_route<')
prefix='''#![allow(dead_code,non_snake_case,non_upper_case_globals,unused_variables)]
use cua_driver_core::image_utils::{CapturedImage, RgbaFrame, ScreenshotEncoding};
use std::time::Duration;
use std::cell::RefCell;
type CFTypeRef=*const ();
type AXUIElementRef=*const ();
const kAXErrorSuccess:i32=0;
#[derive(Debug,PartialEq)] struct PressedItem{is_save:bool}
unsafe fn item_is_save(_:AXUIElementRef)->bool{false}
#[derive(Default)] struct World { key:u32, main:u32, lookup_swap:bool, refuse:bool, ignored_window_focus:bool, enabled:bool, a:usize, b:usize, focused:usize, sibling_deletes:usize, ignored:bool, swap:bool, keys:usize }
thread_local! {static W:RefCell<World>=RefCell::new(World::default());}
fn setup(){ W.with(|w| *w.borrow_mut()=World{key:2,main:2,enabled:true,focused:1,..World::default()}); }
// A is confirmed for lookup; a sibling becomes key before recipient admission.
// The real route must re-establish A, dispatch only there, then restore B.
fn menu_context(){setup();W.with(|w|{let mut w=w.borrow_mut();w.key=1;w.main=1;w.lookup_swap=true;});}
fn AXUIElementCreateApplication(_:i32)->AXUIElementRef{1 as _}
fn set_messaging_timeout(_:AXUIElementRef){}
fn copy_element_attr(_:AXUIElementRef,name:&str)->Option<AXUIElementRef>{Some(W.with(|w|{let w=w.borrow();match name{"AXFocusedWindow"=>(w.key+10) as usize as _,"AXMainWindow"=>(w.main+10) as usize as _,_=>1 as _}}))}
fn ax_get_window_id(window:AXUIElementRef)->Option<u32>{Some(window as usize as u32-10)}
fn CFRelease(_:CFTypeRef){}
fn resolve_exact_prefix(_:AXUIElementRef,_:&[String])->Result<AXUIElementRef,String>{W.with(|w|{let mut w=w.borrow_mut();if w.lookup_swap{w.key=2;}});Ok(1 as _)}
fn copy_bool_attr(_:AXUIElementRef,_:&str)->Option<bool>{Some(W.with(|w|w.borrow().enabled))}
fn copy_action_names(_:AXUIElementRef)->Vec<String>{vec!["AXPress".into()]}
fn choose_action<'a>(_: &'a [String],_:bool)->Option<&'a str>{Some("AXPress")}
fn perform_action(_:AXUIElementRef,_:&str)->i32{W.with(|w|{let mut w=w.borrow_mut();if w.key==1{w.a+=1}else{w.b+=1}});0}
mod ax {pub mod bindings {pub fn focused_window_id_of_pid(_:i32)->Option<u32>{Some(super::super::W.with(|w|w.borrow().key))}}}
mod window_change_detector {pub fn intentional_epoch()->u64{0}}
mod key_focus_guard {pub fn user_switched_recently()->bool{false}}
mod input {
 pub mod skylight {
  pub fn activate_without_raise_with(_:i32,id:u32,_:bool)->bool{super::super::W.with(|w|{let mut w=w.borrow_mut();if w.refuse {false}else{if !w.ignored_window_focus{w.key=id;}true}})}
  pub fn front_process_pid()->Option<i32>{Some(9)}
  pub fn restore_focus_after_without_raise(_:i32,_:Option<u32>,_:i32,_:u32)->bool{true}
 }
 pub mod ax_actions { pub fn focus_element(_:usize)->Result<(),()> {super::super::W.with(|w|{let mut w=w.borrow_mut();if !w.ignored{w.focused=1}});Ok(())} }
 pub mod keyboard {pub fn press_key(_:i32,key:&str,_:&[()])->Result<(),()> {super::super::W.with(|w|{let mut w=w.borrow_mut();w.keys+=1;if w.focused==2 {w.b+=1;if key=="delete"{w.sibling_deletes+=1;}}if key=="space"&&w.swap{w.focused=2}});Ok(())}}
}
#[derive(Debug,PartialEq)] enum DocumentEdited{NotADocument,NotMarked,Marked}
unsafe fn in_document_window(_:AXUIElementRef)->bool{true}
unsafe fn keys_reach_element_window(_:i32,_:AXUIElementRef)->bool{true}
unsafe fn caret_to_end_of(_:AXUIElementRef){}
fn copy_string_attr(_:AXUIElementRef,_:&str)->Option<String>{Some("unchanged target".into())}
'''
prefix+='#[derive(Debug,PartialEq)]\n'+block(menu_source,'enum BackgroundPressError {')+'\n'
for needle in ['impl From<String> for BackgroundPressError {','impl From<&str> for BackgroundPressError {','fn require_menu_window(','unsafe fn require_live_menu_window(']:
 prefix+=block(menu_source,needle)+'\n'
for name in ['ENABLE_WAIT','ENABLE_POLL']:
 prefix+=next(line for line in menu_source.splitlines() if line.startswith('const '+name+':'))+'\n'
prefix+= '#[derive(Debug)]\n'+block(cap,'pub struct WindowCapture {')+'\n'
prefix+=block(cap,'impl WindowCapture {')+'\n'
prefix+=block(cap,'fn png(bytes: Vec<u8>)')+'\n'
prefix+='fn capture_qualified(id:u32,off:bool)->anyhow::Result<WindowCapture>{capture_window_with_space_route(id,off, |_|Ok(png(vec![1])), |_|Ok(png(vec![2])))}\n'
tests=r'''
#[test] fn sibling_menu_context_refuses_before_lookup(){setup();assert_eq!(unsafe{invoke_path_in_background(7,1,&["Edit".into()])},Err(BackgroundPressError::UnboundWindow{requested:1,focused:Some(2),main:Some(2)}));W.with(|w|assert_eq!(w.borrow().a+w.borrow().b,0));}
#[test] fn sibling_main_window_refuses_even_when_focused_window_matches(){menu_context();W.with(|w|w.borrow_mut().main=2);assert_eq!(unsafe{invoke_path_in_background(7,1,&["Edit".into()])},Err(BackgroundPressError::UnboundWindow{requested:1,focused:Some(1),main:Some(2)}));W.with(|w|assert_eq!(w.borrow().a+w.borrow().b,0));}
#[test] fn enabled_menu_targets_a_restores_b(){menu_context();unsafe{invoke_path_in_background(7,1,&["Edit".into()]).unwrap()};W.with(|w|{let w=w.borrow();assert_eq!(w.b,0);assert_eq!(w.a,1);assert_eq!(w.key,2)});}
#[test] fn refused_menu_focus_has_exact_error_and_zero_mutation(){menu_context();W.with(|w|w.borrow_mut().refuse=true);let r=unsafe{invoke_path_in_background(7,1,&["Edit".into()])};assert_eq!(r,Err("invoke_menu: exact background window recipient could not be established".into()));W.with(|w|assert_eq!(w.borrow().a+w.borrow().b,0));}
#[test] fn ignored_ax_focus_selected_sibling_is_untouched(){setup();W.with(|w|{let mut w=w.borrow_mut();w.ignored=true;w.focused=2;});let r=unsafe{ensure_marked_after_ax_write(7,1 as _,false)};W.with(|w|assert_eq!(w.borrow().keys,0));assert_eq!(r,DocumentEdited::NotMarked);}
#[test] fn swap_after_space_never_deletes_sibling(){setup();W.with(|w|w.borrow_mut().swap=true);let r=unsafe{ensure_marked_after_ax_write(7,1 as _,false)};W.with(|w|assert_eq!(w.borrow().sibling_deletes,0));assert_eq!(r,DocumentEdited::NotMarked);}
#[test] fn initial_current_then_offspace_capture_cannot_publish(){let c=capture_qualified(1,true).unwrap();assert!(!c.actionable);}
#[test] fn current_space_public_capture_is_qualified(){let c=capture_qualified(1,false).unwrap();assert!(c.actionable);assert_eq!(c.image,png(vec![1]));}
#[test] fn backing_store_fallback_is_unqualified(){let c=capture_window_with_space_route(1,false, |_|Err(anyhow::anyhow!("unavailable")), |_|Ok(png(vec![2]))).unwrap();assert!(!c.actionable);assert_eq!(c.image,png(vec![2]));}
'''
(out/'src').mkdir(exist_ok=True)
(out/'src/lib.rs').write_text(prefix+menu+'\n'+doc+'\n'+route+'\n'+tests)
(out/'Cargo.toml').write_text(f'''[package]
name="cua-macos-authority-seams"
version="0.0.0"
edition="2021"
[dependencies]
anyhow="1"
image={{version="0.25",default-features=false,features=["png","jpeg"]}}
cua-driver-core={{path="{repo}/libs/cua-driver/rust/crates/cua-driver-core"}}
''')
gws=(root/'tools/get_window_state.rs').read_text()
a=gws.index('        let refreshed = match');b=gws.index('        let (snapshot_id, replaced)',a)
publish=gws[a:b].replace('self\n                .state','state').replace('self.state','state').replace('return ToolResult::error(format!("screenshot refresh refused: {error}"))\n                        .with_structured(serde_json::json!({"code":"snapshot_publication_failed"}))','panic!("{error}")').replace('crate::ax::snapshot::AxSnapshot::from_nodes(&[])','0')
extra='''
#[derive(Clone)] struct Ticket;
struct Context {snapshot_id:u64}
#[derive(Default)] struct Snapshots{ writes:std::cell::Cell<usize>, frame:std::cell::Cell<u64> }
impl Snapshots {
 fn write(&self)->Result<Option<(u64,Vec<u64>)>,String>{self.writes.set(self.writes.get()+1);self.frame.set(99);Ok(Some((99,vec![])))}
 fn refresh_screenshot_for_session(&self,_:Ticket,_:f64,_:Option<(f64,f64)>)->Result<Option<Context>,String>{self.write()?;Ok(Some(Context{snapshot_id:99}))}
 fn try_publish_sized_for_session(&self,_:i32,_:u64,_:u64,_:Option<&str>,_:Option<f64>,_:Option<(f64,f64)>)->Result<Option<(u64,Vec<u64>)>,String>{self.write()}
 fn try_publish_capture_for_refresh(&self,_:Ticket,_:u64,_:f64)->Result<Option<(u64,Vec<u64>)>,String>{self.write()}
 fn try_publish_capture_for_session(&self,_:i32,_:u64,_:u64,_:Option<&str>,_:Option<f64>)->Result<Option<(u64,Vec<u64>)>,String>{self.write()}
}
struct State{snapshots:Snapshots}
fn publication_case(off:bool,refresh:bool){
 let state=State{snapshots:Snapshots::default()};state.snapshots.frame.set(42);
 let pid=7;let window_id=1u32;let session_id:Option<String>=None;
 let scope_matched=true;let want_tree=!refresh;let prepared_snapshot=Some(5u64);
 let refresh_ticket=refresh.then_some(Ticket);let window_size=Some((10.0,10.0));
 let mut observation_only=false;
 let capture_actionable=capture_qualified(1,off).unwrap().actionable;
 QUALIFY
 let screenshot_resize_scale=if observation_only{None}else{Some(1.0)};
 PUBLISH
 let id=publication.unwrap();
 if off {assert!(id.is_none());assert_eq!(state.snapshots.writes.get(),0);assert_eq!(state.snapshots.frame.get(),42)}else{assert!(id.is_some());assert_eq!(state.snapshots.writes.get(),1)}
}
#[test] fn offspace_capture_preserves_snapshot_and_frame(){publication_case(true,false)}
#[test] fn offspace_capture_preserves_refresh_frame(){publication_case(true,true)}
#[test] fn current_capture_publishes(){publication_case(false,false)}
#[test] fn current_capture_refreshes(){publication_case(false,true)}
'''.replace('QUALIFY','observation_only |= !capture_actionable;').replace('PUBLISH',publish)
with (out/'src/lib.rs').open('a') as f:f.write(extra)
# Compile the production capture task and verifier against fake native APIs;
# image parsing, frame validation, resizing, and encoding are real production code.
closure=block(gws,'move || -> Result<').replace('super::px_frame','crate::px_frame')
verified=block(gws,'fn verified_capture(').replace('super::px_frame','crate::px_frame')
px=(root/'tools/px_frame.rs').read_text()
with (out/'src/lib.rs').open('a') as f:
 f.write('\nmod px_frame {\nuse crate::windows::WindowBounds;\n#[derive(Debug, Clone, PartialEq)]\n')
 f.write(block(px,'pub enum PxFrameError {')+'\n'+block(px,'pub fn validate_capture_frame(')+'\n}\n')
 f.write(verified+'\n')
 f.write(r'''
mod windows {
 #[derive(Debug)] pub struct WindowBounds {pub x:f64,pub y:f64,pub width:f64,pub height:f64}
 pub fn window_bounds_by_id(_:u32)->Option<WindowBounds>{Some(WindowBounds{x:0.0,y:0.0,width:10.0,height:10.0})}
}
mod capture {
 pub use cua_driver_core::image_utils::CapturedImage;
 pub use crate::WindowCapture;
 thread_local! {pub static NEXT:std::cell::RefCell<Option<WindowCapture>>=const {std::cell::RefCell::new(None)};}
 pub fn screenshot_window_capture(_:u32)->anyhow::Result<WindowCapture>{Ok(NEXT.with(|next|next.borrow_mut().take().expect("fresh capture fixture")))}
}
fn raw_image(w:u32,h:u32)->CapturedImage {
 CapturedImage::Rgba(RgbaFrame{width:w,height:h,rgba:[11,22,33,255].repeat((w*h) as usize)})
}
fn capture_task(image:CapturedImage,off:bool,encoding:ScreenshotEncoding,out_file:Option<String>)
 -> Result<(bool,Result<(Vec<u8>,Option<String>,u32,u32,u32,u32,windows::WindowBounds,f64),px_frame::PxFrameError>),px_frame::PxFrameError> {
 capture::NEXT.with(|next|*next.borrow_mut()=Some(WindowCapture{image,actionable:!off}));
 let window_id=1;let max_dim=5;let prefetched_raw=None;
 (CLOSURE)()
}
#[test] fn capture_task_preserves_qualification_on_validation_error(){
 for off in [false,true] {
  let (actionable,frame)=capture_task(png(vec![0]),off,ScreenshotEncoding::Png,None).unwrap();
  assert_eq!(actionable,!off);
  assert_eq!(frame.unwrap_err(),px_frame::PxFrameError::CaptureUnavailable{window_id:1,reason:"PNG data too small".into()});
 }
}
#[test] fn capture_task_preserves_qualification_on_frame_mismatch(){
 for off in [false,true] {
  let (actionable,frame)=capture_task(raw_image(3,10),off,ScreenshotEncoding::Png,None).unwrap();
  assert_eq!(actionable,!off);
  assert_eq!(frame.unwrap_err(),px_frame::PxFrameError::FrameMismatch{window_id:1,bounds_width:10.0,bounds_height:10.0,capture_width:3,capture_height:10,scale_x:0.3,scale_y:1.0});
 }
}
#[test] fn capture_task_encodes_png_and_jpeg_from_png_and_rgba(){
 let raw=raw_image(10,10);let backend_png=png(raw.clone().into_png().unwrap());
 for image in [raw,backend_png] {for off in [false,true] {for encoding in [ScreenshotEncoding::Png,ScreenshotEncoding::Jpeg{quality:85}] {
  let (actionable,frame)=capture_task(image.clone(),off,encoding,None).unwrap();
  let (bytes,path,w,h,orig_w,orig_h,bounds,scale)=frame.unwrap();
  assert_eq!(actionable,!off);assert!(path.is_none());
  assert_eq!((w,h,orig_w,orig_h),(5,5,10,10));assert_eq!((bounds.width,bounds.height,scale),(10.0,10.0,1.0));
  let format=if encoding==ScreenshotEncoding::Png{image::ImageFormat::Png}else{image::ImageFormat::Jpeg};
  assert_eq!(image::guess_format(&bytes).unwrap(),format);
  let decoded=image::load_from_memory_with_format(&bytes,format).unwrap();
  assert_eq!((decoded.width(),decoded.height()),(5,5));
 }}}
}
#[test] fn capture_task_preserves_qualification_on_encoding_error(){
 for off in [false,true] {for encoding in [ScreenshotEncoding::Png,ScreenshotEncoding::Jpeg{quality:85}] {
  let image=CapturedImage::Rgba(RgbaFrame{width:10,height:10,rgba:vec![1]});
  let (actionable,frame)=capture_task(image,off,encoding,None).unwrap();
  assert_eq!(actionable,!off);
  assert_eq!(frame.unwrap_err(),px_frame::PxFrameError::CaptureUnavailable{window_id:1,reason:"invalid RGBA buffer for w=10 h=10".into()});
 }}
}
#[test] fn capture_task_preserves_qualification_on_file_output_error(){
 for off in [false,true] {
  // Writing to an existing directory fails without changing any file.
  let (actionable,frame)=capture_task(raw_image(10,10),off,ScreenshotEncoding::Png,Some(".".into())).unwrap();
  assert_eq!(actionable,!off);
  match frame.unwrap_err(){px_frame::PxFrameError::CaptureUnavailable{window_id,reason}=>{assert_eq!(window_id,1);assert!(!reason.is_empty())},other=>panic!("wrong refusal: {other:?}")}
 }
}
#[test] fn byte_only_consumer_refuses_backing_store(){assert_eq!(capture_qualified(1,true).unwrap().into_actionable_bytes().unwrap_err().to_string(),"window capture is observation-only: unqualified Space or backing-store route");}
#[test] fn byte_only_consumer_accepts_current_public(){assert_eq!(capture_qualified(1,false).unwrap().into_actionable_bytes().unwrap(),vec![1]);}
#[test] fn disabled_menu_focus_refusal_is_exact(){menu_context();W.with(|w|{let mut w=w.borrow_mut();w.enabled=false;w.refuse=true;});assert_eq!(unsafe{invoke_path_in_background(7,1,&["Edit".into()])},Err("invoke_menu: exact background window recipient could not be established".into()));W.with(|w|assert_eq!(w.borrow().b,0));}
'''.replace('CLOSURE',closure))
 # Retain the source-owned raw/PNG invalid-frame and prefetch recovery cases.
 for name in ['verified_capture_keeps_unqualified_status_on_invalid_frames','verified_capture_reuses_or_recovers_prefetch']:
  f.write('\n#[test]\n'+block(gws,'fn '+name+'(').replace('super::super::px_frame','crate::px_frame'))

with (out / "src/lib.rs").open("a") as f:
 f.write(r'''
#[test] fn ignored_successful_focus_request_refuses_dispatch(){menu_context();W.with(|w|w.borrow_mut().ignored_window_focus=true);assert_eq!(unsafe{invoke_path_in_background(7,1,&["Edit".into()])},Err("invoke_menu: exact background window recipient could not be established".into()));W.with(|w|assert_eq!(w.borrow().a+w.borrow().b,0));}
''')

# Keep the original native-route tests in this host-free suite too.
with (out / "src/lib.rs").open("a") as f:
 f.write("\nuse std::cell::Cell;\n")
 for name in ["off_space_window_uses_skylight_before_public_backends", "off_space_skylight_failure_falls_back_to_public_backends", "on_space_window_uses_public_backends_and_skylight_last", "raw_route_preserves_pixels_and_byte_consumers_require_qualification", "space_route_errors_keep_every_backend_context"]:
  f.write("\n#[test]\n" + block(cap, "fn " + name + "("))
# The publication result controls all downstream authorities in production.
assert gws.index("observation_only |= !capture_actionable;") < gws.index("let removed = if")
assert "let capture_id = match (snapshot_id, screenshot.as_ref().filter(|_| !jpeg))" in gws
assert "(Some(sid), Some(r)) => build_elements_array_with_token" in gws
assert 'serde_json::json!(!observation_only)' in gws
# Exercise the production capture-publication selector (not a replacement JPEG
# predicate). The fake result records eligibility, not a real registry publish.
a=gws.index('let capture_id = match ')+len('let capture_id = match ')
b=gws.index(' {',a)
selector=gws[a:b]
with (out/'src/lib.rs').open('a') as f:
 f.write(r'''
fn capture_publication_eligible(snapshot_id:Option<u64>,screenshot:Option<&Vec<u8>>,jpeg:bool)->Option<u64>{
 match SELECTOR {(Some(sid),Some(_))=>Some(sid),_=>None}
}
#[test] fn only_png_with_snapshot_and_screenshot_can_publish_capture_id(){
 let screenshot=vec![1];
 for snapshot_id in [None,Some(42)] {for screenshot in [None,Some(&screenshot)] {
  assert_eq!(capture_publication_eligible(snapshot_id,screenshot,false),snapshot_id.filter(|_|screenshot.is_some()));
  assert_eq!(capture_publication_eligible(snapshot_id,screenshot,true),None);
 }}
}
'''.replace('SELECTOR',selector))

# Scripting routes use exact application identity and readback, never a missing
# observation as evidence that a write happened. Exercise the production predicates.
script_source = (root / 'apple_events.rs').read_text()
with (out / 'src/lib.rs').open('a') as f:
 f.write('\n' + block(script_source, 'fn unique_app_matches(') + '\n' + block(script_source, 'fn mtime_advanced('))
 f.write(r'''
#[test] fn script_target_is_one_exact_process(){assert!(unique_app_matches(7,&[7]));for pids in [&[][..],&[8][..],&[7,8][..],&[7,7][..]]{assert!(!unique_app_matches(7,pids));}}
#[test] fn unreadable_mtime_is_not_write_evidence(){let a=std::time::SystemTime::UNIX_EPOCH;let b=a+Duration::from_secs(1);assert!(mtime_advanced(Some(a),Some(b)));assert!(!mtime_advanced(None,Some(b)));assert!(!mtime_advanced(Some(a),None));assert!(!mtime_advanced(Some(a),Some(a)));assert!(!mtime_advanced(Some(b),Some(a)));}
''')
