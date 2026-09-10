use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::adw::subclass::prelude::*;
use crate::window::MachinesWindow;
use crate::{adw, config, gdk, gio, glib, gtk, prefs};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct MachinesApplication;

    #[glib::object_subclass]
    impl ObjectSubclass for MachinesApplication {
        const NAME: &'static str = "MachinesApplication";
        type Type = super::MachinesApplication;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for MachinesApplication {}

    impl ApplicationImpl for MachinesApplication {
        fn startup(&self) {
            self.parent_startup();
            let css = gtk::CssProvider::new();
            css.load_from_resource(&format!("{}/style.css", config::RESOURCE_PATH));
            if let Some(display) = gdk::Display::default() {
                gtk::style_context_add_provider_for_display(
                    &display,
                    &css,
                    gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
                );
            }
            self.obj().setup_actions();
        }

        /// One window per instance: launching again brings it back to the front.
        fn activate(&self) {
            let app = self.obj();
            let window = app
                .active_window()
                .unwrap_or_else(|| MachinesWindow::new(&*app).upcast());
            window.present();
        }
    }

    impl GtkApplicationImpl for MachinesApplication {}
    impl AdwApplicationImpl for MachinesApplication {}
}

glib::wrapper! {
    pub struct MachinesApplication(ObjectSubclass<imp::MachinesApplication>)
        @extends adw::Application, gtk::Application, gio::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl MachinesApplication {
    pub fn new() -> Self {
        glib::Object::builder()
            .property("application-id", config::APP_ID)
            .property("resource-base-path", config::RESOURCE_PATH)
            .build()
    }

    fn setup_actions(&self) {
        let quit = gio::ActionEntry::builder("quit")
            .activate(|app: &Self, _, _| app.quit())
            .build();
        let about = gio::ActionEntry::builder("about")
            .activate(|app: &Self, _, _| app.show_about())
            .build();
        self.add_action_entries([quit, about]);
        self.add_action(&prefs::settings().create_action("connection-uri"));
        self.set_accels_for_action("app.quit", &["<Control>q"]);
        self.set_accels_for_action("window.close", &["<Control>w"]);
        self.set_accels_for_action("win.new-machine", &["<Control>n"]);
    }

    fn show_about(&self) {
        adw::AboutDialog::builder()
            .application_name("Machines")
            .application_icon(config::APP_ID)
            .version(config::VERSION)
            .developer_name("sachesi")
            .license_type(gtk::License::Gpl30)
            .comments(gettext("Create and run libvirt virtual machines."))
            .translator_credits(gettext("translator-credits"))
            .website("https://github.com/sachesi/machines")
            .issue_url("https://github.com/sachesi/machines/issues")
            .build()
            .present(self.active_window().as_ref());
    }
}
