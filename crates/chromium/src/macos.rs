use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use objc2::{DefinedClass, define_class, extern_methods, msg_send, rc::Retained, runtime::Bool};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEvent};
use std::cell::Cell;

define_class! {
    #[unsafe(super(NSApplication))]
    #[ivars = Cell<Bool>]
    struct BrowserApplication;
    impl BrowserApplication {
        #[unsafe(method(sendEvent:))]
        unsafe fn send_event(&self,event:&NSEvent) {
            let previous=self.ivars().replace(Bool::YES);
            unsafe {let _:()=msg_send![super(self),sendEvent:event];}
            self.ivars().set(previous);
        }
    }
    unsafe impl CrAppProtocol for BrowserApplication {
        #[unsafe(method(isHandlingSendEvent))]
        unsafe fn handling(&self)->Bool {self.ivars().get()}
    }
    unsafe impl CrAppControlProtocol for BrowserApplication {
        #[unsafe(method(setHandlingSendEvent:))]
        unsafe fn set_handling(&self,value:Bool){self.ivars().set(value);}
    }
    unsafe impl CefAppProtocol for BrowserApplication {}
}
impl BrowserApplication {
    extern_methods! {#[unsafe(method(sharedApplication))] fn shared_application()->Retained<Self>;}
}
pub fn initialize() {
    let app = BrowserApplication::shared_application();
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}
