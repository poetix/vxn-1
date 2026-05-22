//! In-process host smoke test: loads the VXN1 entry, checks the factory
//! descriptor, instantiates the plugin, and activates the audio processor.
//! Proves the CLAP FFI surface (entry, factory, extensions, activate) is sound.
//! DSP correctness is covered by vxn-engine's unit tests.

use clack_host::factory::plugin::PluginFactory;
use clack_host::prelude::*;
use clack_plugin::clack_entry;
use clack_plugin::prelude::*;
use vxn_clap::VxnPlugin;

static VXN_ENTRY: EntryDescriptor = clack_entry!(SinglePluginEntry<VxnPlugin>);

struct TestHostShared;
impl SharedHandler<'_> for TestHostShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

struct TestHost;
impl HostHandlers for TestHost {
    type Shared<'a> = TestHostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

#[test]
fn entry_factory_descriptor_is_correct() {
    // SAFETY: descriptor comes from clack's own macro.
    let entry = unsafe { PluginEntry::load_from_raw(&VXN_ENTRY, c"/tmp/VXN1.clap") }.unwrap();
    let factory = entry.get_factory::<PluginFactory>().unwrap();
    assert_eq!(factory.plugin_count(), 1);
    let desc = factory.plugin_descriptor(0).unwrap();
    assert_eq!(desc.id().unwrap().to_str().unwrap(), "labs.vulpus.vxn1");
    assert_eq!(desc.name().unwrap().to_str().unwrap(), "VXN1");
}

#[test]
fn instantiates_and_activates() {
    let entry = unsafe { PluginEntry::load_from_raw(&VXN_ENTRY, c"/tmp/VXN1.clap") }.unwrap();
    let host_info =
        HostInfo::new("VXN Test", "Vulpus Labs", "https://vulpus.labs", "0.1.0").unwrap();

    let mut instance = PluginInstance::<TestHost>::new(
        |_| TestHostShared,
        |_| (),
        &entry,
        c"labs.vulpus.vxn1",
        &host_info,
    )
    .expect("instantiation failed");

    let config = PluginAudioConfiguration {
        sample_rate: 48_000.0,
        min_frames_count: 1,
        max_frames_count: 512,
    };
    let _processor = instance
        .activate(|_, _| (), config)
        .expect("activation failed");
}
