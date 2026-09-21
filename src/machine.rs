//! `MachinesMachine`: one virtual machine in the sidebar's list model.

use std::cell::{Cell, RefCell};
use std::sync::OnceLock;

use crate::adw::prelude::*;
use crate::adw::subclass::prelude::*;
use crate::glib;
use crate::hypervisor::{MachineInfo, MachineState};

mod imp {
    use super::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::Machine)]
    pub struct Machine {
        #[property(get, construct_only)]
        uuid: RefCell<String>,
        #[property(get)]
        pub(super) name: RefCell<String>,
        #[property(get, builder(MachineState::default()))]
        pub(super) state: Cell<MachineState>,
        /// The state as the user reads it.
        #[property(get)]
        pub(super) status: RefCell<String>,
        pub(super) info: RefCell<Option<MachineInfo>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Machine {
        const NAME: &'static str = "MachinesMachine";
        type Type = super::Machine;
    }

    #[glib::derived_properties]
    impl ObjectImpl for Machine {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: OnceLock<Vec<glib::subclass::Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| vec![glib::subclass::Signal::builder("changed").build()])
        }
    }
}

glib::wrapper! {
    pub struct Machine(ObjectSubclass<imp::Machine>);
}

impl Machine {
    pub fn new(info: MachineInfo) -> Self {
        let machine: Self = glib::Object::builder().property("uuid", &info.uuid).build();
        machine.update(info);
        machine
    }

    pub fn info(&self) -> Option<MachineInfo> {
        self.imp().info.borrow().clone()
    }

    /// Take in what the latest listing says; `changed` is emitted only if anything did.
    pub fn update(&self, info: MachineInfo) {
        let imp = self.imp();
        if imp.info.borrow().as_ref() == Some(&info) {
            return;
        }
        if *imp.name.borrow() != info.name {
            imp.name.replace(info.name.clone());
            self.notify_name();
        }
        if imp.state.get() != info.state {
            imp.state.set(info.state);
            self.notify_state();
        }
        let status = info.status();
        if *imp.status.borrow() != status {
            imp.status.replace(status);
            self.notify_status();
        }
        imp.info.replace(Some(info));
        self.emit_by_name::<()>("changed", &[]);
    }

    pub fn connect_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("changed", false, move |args| {
            f(&args[0].get().expect("a Machine"));
            None
        })
    }
}
