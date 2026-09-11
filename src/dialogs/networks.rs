//! "Virtual Networks": libvirt's networks on the connection, each with a page of its own.

use std::cell::RefCell;
use std::rc::Rc;

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::details::info_row;
use crate::dialogs::{self, add_button};
use crate::host_xml::{Ipv4Subnet, NetworkConfig, NewNetwork};
use crate::hypervisor::{Hypervisor, Result, VirtualNetwork};
use crate::window::MachinesWindow;
use crate::{adw, glib, gtk};

/// Holds the dialog's widgets weakly, as the storage dialog does.
struct Networks {
    win: MachinesWindow,
    dialog: glib::WeakRef<adw::PreferencesDialog>,
    page: glib::WeakRef<adw::PreferencesPage>,
    list: glib::WeakRef<adw::PreferencesGroup>,
    /// The network whose page is open, by UUID.
    open: RefCell<Option<(String, glib::WeakRef<adw::NavigationPage>)>>,
    networks: RefCell<Vec<VirtualNetwork>>,
}

pub fn present(win: &MachinesWindow) {
    let dialog = adw::PreferencesDialog::builder()
        .title(gettext("Virtual Networks"))
        .content_height(620)
        .build();
    let page = adw::PreferencesPage::new();
    dialog.add(&page);
    let networks = Rc::new(Networks {
        win: win.clone(),
        dialog: dialog.downgrade(),
        page: page.downgrade(),
        list: glib::WeakRef::new(),
        open: RefCell::default(),
        networks: RefCell::default(),
    });
    networks.reload();
    dialog.present(Some(win));
}

fn mode(config: &NetworkConfig) -> String {
    match config.forward.as_deref() {
        None => gettext("Isolated"),
        Some("nat") => gettext("NAT"),
        Some("route") => gettext("Routed"),
        Some("open") => gettext("Open"),
        Some("bridge") => match &config.bridge {
            Some(bridge) => gettext("Host Bridge {name}").replace("{name}", bridge),
            None => gettext("Host Bridge"),
        },
        Some(other) => other.to_owned(),
    }
}

impl Networks {
    fn reload(self: &Rc<Self>) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Some(networks) = this.win.call(|hv| hv.networks()).await {
                this.show(networks);
            }
        });
    }

    fn act<F>(self: &Rc<Self>, f: F)
    where
        F: FnOnce(&Hypervisor) -> Result<()> + Send + 'static,
    {
        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Some(Err(e)) = this.win.call(f).await
                && let Some(dialog) = this.dialog.upgrade()
            {
                dialog.add_toast(adw::Toast::new(&e));
            }
            this.reload();
        });
    }

    fn show(self: &Rc<Self>, networks: Result<Vec<VirtualNetwork>>) {
        let (Some(dialog), Some(page)) = (self.dialog.upgrade(), self.page.upgrade()) else {
            return;
        };
        if let Some(old) = self.list.upgrade() {
            page.remove(&old);
        }
        let group = adw::PreferencesGroup::builder()
            .title(gettext("Virtual Networks"))
            .build();
        let add = add_button(&gettext("New Virtual Network"));
        add.connect_clicked(glib::clone!(
            #[strong(rename_to = this)]
            self,
            move |_| this.new_network()
        ));
        group.set_header_suffix(Some(&add));
        let networks = match networks {
            Ok(networks) => networks,
            Err(e) => {
                group.set_description(Some(&e));
                Vec::new()
            }
        };
        if networks.is_empty() && group.description().is_none() {
            group.set_description(Some(&gettext("No virtual networks")));
        }
        for net in &networks {
            let mut subtitle = mode(&net.config);
            if let Some(ipv4) = net.config.ipv4 {
                subtitle = format!("{subtitle} · {ipv4}");
            }
            if !net.active {
                subtitle = format!("{subtitle} · {}", gettext("Inactive"));
            }
            let row = adw::ActionRow::builder()
                .title(&net.name)
                .subtitle(subtitle)
                .activatable(true)
                .build();
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            row.connect_activated(glib::clone!(
                #[strong(rename_to = this)]
                self,
                #[strong]
                net,
                move |_| this.open_network(&net)
            ));
            group.add(&row);
        }
        page.add(&group);
        self.list.set(Some(&group));

        let open = self.open.borrow().clone();
        if let Some((uuid, sub)) = open
            && let Some(sub) = sub.upgrade()
        {
            match networks.iter().find(|n| n.uuid == uuid) {
                Some(net) => sub.set_child(Some(&self.network_page(net))),
                None => {
                    dialog.pop_subpage();
                }
            }
        }
        self.networks.replace(networks);
    }

    fn open_network(self: &Rc<Self>, net: &VirtualNetwork) {
        let Some(dialog) = self.dialog.upgrade() else {
            return;
        };
        let sub = adw::NavigationPage::builder()
            .title(&net.name)
            .child(&self.network_page(net))
            .build();
        sub.connect_hidden(glib::clone!(
            #[strong(rename_to = this)]
            self,
            move |_| {
                this.open.take();
            }
        ));
        self.open.replace(Some((net.uuid.clone(), sub.downgrade())));
        dialog.push_subpage(&sub);
    }

    /// The machines with an interface on `name`.
    fn users(&self, name: &str) -> Vec<String> {
        self.win
            .machine_infos()
            .into_iter()
            .filter(|m| {
                m.config.as_ref().is_some_and(|c| {
                    c.nics
                        .iter()
                        .any(|n| n.kind == "network" && n.source.as_deref() == Some(name))
                })
            })
            .map(|m| m.name)
            .collect()
    }

    fn network_page(self: &Rc<Self>, net: &VirtualNetwork) -> adw::ToolbarView {
        let page = adw::PreferencesPage::new();
        let overview = adw::PreferencesGroup::new();
        overview.add(&info_row(&gettext("Mode"), &mode(&net.config)));
        if let Some(bridge) = &net.config.bridge {
            overview.add(&info_row(&gettext("Bridge"), bridge));
        }
        if let Some(ipv4) = net.config.ipv4 {
            overview.add(&info_row(&gettext("Host Address"), &ipv4.to_string()));
        }
        if let Some((start, end)) = &net.config.dhcp {
            overview.add(&info_row(
                &gettext("DHCP Range"),
                &format!("{start} – {end}"),
            ));
        }
        let users = self.users(&net.name);
        if !users.is_empty() {
            overview.add(&info_row(&gettext("Used By"), &users.join(", ")));
        }
        let active = adw::SwitchRow::builder()
            .title(gettext("Active"))
            .active(net.active)
            .build();
        let uuid = net.uuid.clone();
        active.connect_active_notify(glib::clone!(
            #[strong(rename_to = this)]
            self,
            move |row| {
                let (uuid, on) = (uuid.clone(), row.is_active());
                this.act(move |hv| hv.set_network_active(&uuid, on));
            }
        ));
        overview.add(&active);
        if net.persistent {
            let autostart = adw::SwitchRow::builder()
                .title(gettext("Start With the Host"))
                .active(net.autostart)
                .build();
            let uuid = net.uuid.clone();
            autostart.connect_active_notify(glib::clone!(
                #[strong(rename_to = this)]
                self,
                move |row| {
                    let (uuid, on) = (uuid.clone(), row.is_active());
                    this.act(move |hv| hv.set_network_autostart(&uuid, on));
                }
            ));
            overview.add(&autostart);
        }
        page.add(&overview);

        let delete = adw::ButtonRow::builder()
            .title(gettext("_Delete Network"))
            .use_underline(true)
            .build();
        delete.add_css_class("destructive-action");
        delete.connect_activated(glib::clone!(
            #[strong(rename_to = this)]
            self,
            #[strong]
            net,
            move |row| {
                let this = this.clone();
                let net = net.clone();
                let row = row.clone();
                let users = users.clone();
                glib::spawn_future_local(async move {
                    let heading = gettext("Delete “{name}”?").replace("{name}", &net.name);
                    let body = if users.is_empty() {
                        gettext("The network stops, and its definition is deleted.")
                    } else {
                        gettext(
                            "The network stops, and its definition is deleted. {machines} \
                             will not start until their interface on it is removed.",
                        )
                        .replace("{machines}", &users.join(", "))
                    };
                    if dialogs::confirm(&row, &heading, &body, &gettext("_Delete")).await {
                        this.act(move |hv| hv.remove_network(&net.uuid));
                    }
                });
            }
        ));
        let danger = adw::PreferencesGroup::new();
        danger.add(&delete);
        page.add(&danger);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&page));
        toolbar
    }

    fn new_network(self: &Rc<Self>) {
        let Some(parent) = self.dialog.upgrade() else {
            return;
        };
        let (taken, subnets): (Vec<String>, Vec<Ipv4Subnet>) = {
            let networks = self.networks.borrow();
            (
                networks.iter().map(|n| n.name.clone()).collect(),
                networks.iter().filter_map(|n| n.config.ipv4).collect(),
            )
        };
        let name = adw::EntryRow::builder()
            .title(gettext("_Name"))
            .use_underline(true)
            .build();
        let nat = adw::ComboRow::builder()
            .title(gettext("_Mode"))
            .use_underline(true)
            .model(&gtk::StringList::new(&[
                &gettext("NAT"),
                &gettext("Isolated"),
            ]))
            .build();
        let explain = |nat: &adw::ComboRow| {
            nat.set_subtitle(&if nat.selected() == 0 {
                gettext("Guests reach outside through the host")
            } else {
                gettext("Guests reach only the host and each other")
            });
        };
        explain(&nat);
        nat.connect_selected_notify(explain);
        let subnet = adw::EntryRow::builder()
            .title(gettext("IPv4 _Network"))
            .use_underline(true)
            .text(Ipv4Subnet::unused(&subnets).to_string())
            .build();
        let dhcp = adw::SwitchRow::builder()
            .title(gettext("_DHCP"))
            .subtitle(gettext("Give guests their addresses"))
            .use_underline(true)
            .active(true)
            .build();
        let group = adw::PreferencesGroup::new();
        group.add(&name);
        group.add(&nat);
        group.add(&subnet);
        group.add(&dhcp);
        let page = adw::PreferencesPage::new();
        page.add(&group);
        let (dialog, create) =
            dialogs::form(&gettext("New Virtual Network"), &gettext("C_reate"), &page);

        let request = Rc::new(glib::clone!(
            #[weak]
            name,
            #[weak]
            nat,
            #[weak]
            subnet,
            #[weak]
            dhcp,
            #[upgrade_or]
            None,
            move || {
                let text = name.text().trim().to_owned();
                let name_ok = !text.is_empty() && !text.contains('/') && !taken.contains(&text);
                let parsed = Ipv4Subnet::parse_network(&subnet.text());
                for (row, ok, filled) in [
                    (name.upcast_ref::<gtk::Widget>(), name_ok, !text.is_empty()),
                    (subnet.upcast_ref(), parsed.is_some(), true),
                ] {
                    row.remove_css_class("error");
                    if filled && !ok {
                        row.add_css_class("error");
                    }
                }
                Some(NewNetwork {
                    name: name_ok.then_some(text)?,
                    nat: nat.selected() == 0,
                    subnet: parsed?,
                    dhcp: dhcp.is_active(),
                })
            }
        ));
        for entry in [&name, &subnet] {
            entry.connect_changed(glib::clone!(
                #[weak]
                create,
                #[strong]
                request,
                move |_| create.set_sensitive(request().is_some())
            ));
        }
        create.connect_clicked(glib::clone!(
            #[strong(rename_to = this)]
            self,
            #[weak]
            dialog,
            move |_| {
                if let Some(new) = request() {
                    dialog.close();
                    this.act(move |hv| hv.create_network(&new));
                }
            }
        ));
        dialog.present(Some(&parent));
    }
}
