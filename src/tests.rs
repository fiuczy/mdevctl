#[cfg(test)]
mod tests {
    use crate::Environment;
    use crate::MdevInfo;
    use anyhow::Result;
    use log::info;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use tempdir::TempDir;
    use uuid::Uuid;

    const TEST_DATA_DIR: &str = "tests";

    fn init() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[derive(PartialEq)]
    enum Expect {
        Pass,
        Fail,
    }

    #[derive(Debug)]
    struct TestEnvironment {
        env: Environment,
        datapath: PathBuf,
        scratch: TempDir,
    }

    impl TestEnvironment {
        pub fn new(testname: &str, testcase: &str) -> TestEnvironment {
            let path: PathBuf = [TEST_DATA_DIR, testname, testcase].iter().collect();
            let scratchdir = TempDir::new(format!("mdevctl-{}", testname).as_str()).unwrap();
            let test = TestEnvironment {
                datapath: path,
                env: Environment::new(scratchdir.path().to_str().unwrap()),
                scratch: scratchdir,
            };
            // populate the basic directories in the environment
            fs::create_dir_all(test.env.mdev_base()).expect("Unable to create mdev_base");
            fs::create_dir_all(test.env.persist_base()).expect("Unable to create persist_base");
            fs::create_dir_all(test.env.parent_base()).expect("Unable to create parent_base");
            info!("---- Running test '{}/{}' ----", testname, testcase);
            test
        }

        // set up a few files in the test environment to simulate an defined mediated device
        fn populate_defined_device(&self, uuid: &str, parent: &str, filename: &str) {
            let jsonfile = self.datapath.join(filename);
            let parentdir = self.env.persist_base().join(parent);
            fs::create_dir_all(&parentdir).expect("Unable to setup parent dir");
            let deffile = parentdir.join(uuid);
            assert!(jsonfile.exists());
            assert!(!deffile.exists());
            fs::copy(jsonfile, deffile).expect("Unable to copy device def");
        }

        // set up a few files in the test environment to simulate an active mediated device
        fn populate_active_device(&self, uuid: &str, parent: &str, mdev_type: &str) {
            use std::os::unix::fs::symlink;

            let parentdir = self.env.parent_base().join(parent).join(uuid);
            fs::create_dir_all(&parentdir).expect("Unable to setup mdev parent");

            let mut parenttypedir = self.scratch.path().join("sys/devices/pci0000:00/");
            parenttypedir.push(parent);
            parenttypedir.push("mdev_supported_types");
            parenttypedir.push(mdev_type);
            fs::create_dir_all(&parenttypedir).expect("Unable to setup mdev parent type");

            let devdir = self.env.mdev_base().join(uuid);
            fs::create_dir_all(&devdir.parent().unwrap()).expect("Unable to setup mdev dir");
            symlink(&parentdir, &devdir).expect("Unable to symlink mdev file");

            let typefile = devdir.join("mdev_type");
            symlink(&parenttypedir, &typefile).expect("Unable to setup mdev type");
        }
    }

    fn get_flag(varname: &str) -> bool {
        match env::var(varname) {
            Err(_) => {
                return false;
            }
            Ok(s) => match s.trim().parse::<i32>() {
                Err(_) => return false,
                Ok(n) => return n > 0,
            },
        }
    }

    fn regen(filename: &PathBuf, data: &str) -> Result<()> {
        let parentdir = filename.parent().unwrap();
        fs::create_dir_all(parentdir)?;

        fs::write(filename, data.as_bytes())
            .and_then(|_| {
                println!("Regenerated expected data file {:?}", filename);
                Ok(())
            })
            .map_err(|err| err.into())
    }

    const REGEN_FLAG: &str = "MDEVCTL_TEST_REGENERATE_OUTPUT";

    fn compare_to_file(filename: &PathBuf, actual: &str) {
        let flag = get_flag(REGEN_FLAG);
        if flag {
            regen(filename, actual).expect("Failed to regenerate expected output");
        }
        let expected = fs::read_to_string(filename).unwrap_or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                println!(
                    "File {:?} not found, run tests with {}=1 to automatically \
                             generate expected output",
                    filename, REGEN_FLAG
                );
            }
            Default::default()
        });

        assert_eq!(expected, actual);
    }

    fn load_from_json<'a>(
        env: &'a Environment,
        uuid: &str,
        parent: &str,
        filename: &PathBuf,
    ) -> Result<MdevInfo<'a>> {
        let uuid = Uuid::parse_str(uuid);
        assert!(uuid.is_ok());
        let uuid = uuid.unwrap();
        let mut dev = MdevInfo::new(env, uuid);

        let jsonstr = fs::read_to_string(filename)?;
        let jsonval: serde_json::Value = serde_json::from_str(&jsonstr)?;
        dev.load_from_json(parent.to_string(), &jsonval)?;

        Ok(dev)
    }

    fn test_load_json_helper(uuid: &str, parent: &str) {
        let test = TestEnvironment::new("load-json", uuid);
        let infile = test.datapath.join(format!("{}.in", uuid));
        let outfile = test.datapath.join(format!("{}.out", uuid));

        let dev = load_from_json(&test.env, uuid, parent, &infile).unwrap();
        let jsonval = dev.to_json(false).unwrap();
        let jsonstr = serde_json::to_string_pretty(&jsonval).unwrap();

        compare_to_file(&outfile, &jsonstr);
        assert_eq!(uuid, dev.uuid.to_hyphenated().to_string());
        assert_eq!(parent, dev.parent);
    }

    #[test]
    fn test_load_json() {
        init();

        test_load_json_helper("c07ab7b2-8aa2-427a-91c6-ffc949bb77f9", "0000:00:02.0");
        test_load_json_helper("783e6dbb-ea0e-411f-94e2-717eaad438bf", "0001:00:03.1");
        test_load_json_helper("5269fe7a-18d1-48ad-88e1-3fda4176f536", "0000:00:03.0");
    }

    fn test_define_helper<F>(
        testname: &str,
        expect: Expect,
        uuid: Option<Uuid>,
        auto: bool,
        parent: Option<String>,
        mdev_type: Option<String>,
        jsonfile: Option<PathBuf>,
        setupfn: F,
    ) where
        F: Fn(&TestEnvironment),
    {
        use crate::define_command_helper;
        let test = TestEnvironment::new("define", testname);

        // load the jsonfile from the test path.
        let jsonfile = match jsonfile {
            Some(f) => Some(test.datapath.join(f)),
            None => None,
        };

        setupfn(&test);

        let expectedfile = test.datapath.join("expected");
        let def = define_command_helper(&test.env, uuid, auto, parent, mdev_type, jsonfile);
        if expect == Expect::Fail {
            def.expect_err("expected define command to fail");
            return;
        }

        let def = def.expect("define command failed unexpectedly");
        let path = def.persist_path().unwrap();
        assert!(!path.exists());
        def.define().expect("Failed to define device");
        assert!(path.exists());
        assert!(def.is_defined());
        let filecontents = fs::read_to_string(&path).unwrap();
        compare_to_file(&expectedfile, &filecontents);
    }

    #[test]
    fn test_define() {
        init();

        const DEFAULT_UUID: &str = "976d8cc2-4bfc-43b9-b9f9-f4af2de91ab9";
        const DEFAULT_PARENT: &str = "0000:00:03.0";
        test_define_helper(
            "no-uuid-no-type",
            Expect::Fail,
            None,
            true,
            Some(DEFAULT_PARENT.to_string()),
            None,
            None,
            |_| {},
        );
        // if no uuid is specified, one will be auto-generated
        test_define_helper(
            "no-uuid",
            Expect::Pass,
            None,
            true,
            Some(DEFAULT_PARENT.to_string()),
            Some("i915-GVTg_V5_4".to_string()),
            None,
            |_| {},
        );
        // specify autostart
        test_define_helper(
            "uuid-auto",
            Expect::Pass,
            Uuid::parse_str(DEFAULT_UUID).ok(),
            true,
            Some(DEFAULT_PARENT.to_string()),
            Some("i915-GVTg_V5_4".to_string()),
            None,
            |_| {},
        );
        // specify manual start
        test_define_helper(
            "uuid-manual",
            Expect::Pass,
            Uuid::parse_str(DEFAULT_UUID).ok(),
            false,
            Some(DEFAULT_PARENT.to_string()),
            Some("i915-GVTg_V5_4".to_string()),
            None,
            |_| {},
        );
        // invalid to specify an separate mdev_type if defining via jsonfile
        test_define_helper(
            "jsonfile-type",
            Expect::Fail,
            Uuid::parse_str(DEFAULT_UUID).ok(),
            false,
            Some(DEFAULT_PARENT.to_string()),
            Some("i915-GVTg_V5_4".to_string()),
            Some(PathBuf::from("in.json")),
            |_| {},
        );
        // specifying via jsonfile properly
        test_define_helper(
            "jsonfile",
            Expect::Pass,
            Uuid::parse_str(DEFAULT_UUID).ok(),
            false,
            Some(DEFAULT_PARENT.to_string()),
            None,
            Some(PathBuf::from("in.json")),
            |_| {},
        );
        // If uuid is already active, specifying mdev_type will result in an error
        test_define_helper(
            "uuid-running-no-parent",
            Expect::Fail,
            Uuid::parse_str(DEFAULT_UUID).ok(),
            false,
            None,
            Some("i915-GVTg_V5_4".to_string()),
            None,
            |test| {
                test.populate_active_device(DEFAULT_UUID, DEFAULT_PARENT, "i915-GVTg_V5_4");
            },
        );
        // If uuid is already active, should use mdev_type from running mdev
        test_define_helper(
            "uuid-running-no-type",
            Expect::Pass,
            Uuid::parse_str(DEFAULT_UUID).ok(),
            false,
            Some(DEFAULT_PARENT.to_string()),
            None,
            None,
            |test| {
                test.populate_active_device(DEFAULT_UUID, DEFAULT_PARENT, "i915-GVTg_V5_4");
            },
        );
        // ok to define a device with the same uuid as a running device even if they have different
        // parent devices
        test_define_helper(
            "uuid-running-diff-parent",
            Expect::Pass,
            Uuid::parse_str(DEFAULT_UUID).ok(),
            false,
            Some(DEFAULT_PARENT.to_string()),
            Some("i915-GVTg_V5_4".to_string()),
            None,
            |test| {
                test.populate_active_device(DEFAULT_UUID, "0000:00:02.0", "i915-GVTg_V5_4");
            },
        );
        // ok to define a device with the same uuid as a running device even if they have different
        // mdev_types
        test_define_helper(
            "uuid-running-diff-type",
            Expect::Pass,
            Uuid::parse_str(DEFAULT_UUID).ok(),
            false,
            Some(DEFAULT_PARENT.to_string()),
            Some("i915-GVTg_V5_4".to_string()),
            None,
            |test| {
                test.populate_active_device(DEFAULT_UUID, DEFAULT_PARENT, "different_type");
            },
        );
        // defining a device that is already defined should result in an error
        test_define_helper(
            "uuid-already-defined",
            Expect::Fail,
            Uuid::parse_str(DEFAULT_UUID).ok(),
            false,
            Some(DEFAULT_PARENT.to_string()),
            Some("i915-GVTg_V5_4".to_string()),
            None,
            |test| {
                test.populate_defined_device(DEFAULT_UUID, DEFAULT_PARENT, "defined.json");
            },
        );
    }

    fn test_modify_helper<F>(
        testname: &str,
        expect: Expect,
        uuid: &str,
        parent: Option<String>,
        mdev_type: Option<String>,
        addattr: Option<String>,
        delattr: bool,
        index: Option<u32>,
        value: Option<String>,
        auto: bool,
        manual: bool,
        setupfn: F,
    ) where
        F: Fn(&TestEnvironment),
    {
        use crate::modify_command;
        let test = TestEnvironment::new("modify", testname);
        let expectedfile = test.datapath.join("expected");
        setupfn(&test);
        let uuid = Uuid::parse_str(uuid).unwrap();
        let result = modify_command(
            &test.env,
            uuid,
            parent.clone(),
            mdev_type,
            addattr,
            delattr,
            index,
            value,
            auto,
            manual,
        );
        if expect == Expect::Fail {
            assert!(result.is_err());
            return;
        }

        result.expect("Modify command failed unexpectedly");

        let def = crate::get_defined_device(&test.env, uuid, &parent)
            .expect("Couldn't find defined device");
        let path = def.persist_path().unwrap();
        assert!(path.exists());
        assert!(def.is_defined());
        let filecontents = fs::read_to_string(&path).unwrap();
        compare_to_file(&expectedfile, &filecontents);
    }

    #[test]
    fn test_modify() {
        init();

        const UUID: &str = "976d8cc2-4bfc-43b9-b9f9-f4af2de91ab9";
        const PARENT: &str = "0000:00:03.0";
        test_modify_helper(
            "device-not-defined",
            Expect::Fail,
            UUID,
            None,
            None,
            None,
            false,
            None,
            None,
            false,
            false,
            |_| {},
        );
        test_modify_helper(
            "auto",
            Expect::Pass,
            UUID,
            Some(PARENT.to_string()),
            None,
            None,
            false,
            None,
            None,
            true,
            false,
            |test| {
                test.populate_defined_device(UUID, PARENT, "defined.json");
            },
        );
        test_modify_helper(
            "manual",
            Expect::Pass,
            UUID,
            Some(PARENT.to_string()),
            None,
            None,
            false,
            None,
            None,
            false,
            true,
            |test| {
                test.populate_defined_device(UUID, PARENT, "defined.json");
            },
        );
        test_modify_helper(
            "delattr",
            Expect::Pass,
            UUID,
            Some(PARENT.to_string()),
            None,
            None,
            true,
            Some(2),
            None,
            false,
            false,
            |test| {
                test.populate_defined_device(UUID, PARENT, "defined.json");
            },
        );
        test_modify_helper(
            "delattr-noindex",
            Expect::Pass,
            UUID,
            Some(PARENT.to_string()),
            None,
            None,
            true,
            None,
            None,
            false,
            false,
            |test| {
                test.populate_defined_device(UUID, PARENT, "defined.json");
            },
        );
        test_modify_helper(
            "addattr",
            Expect::Pass,
            UUID,
            Some(PARENT.to_string()),
            None,
            Some("added-attr".to_string()),
            false,
            Some(3),
            Some("added-attr-value".to_string()),
            false,
            false,
            |test| {
                test.populate_defined_device(UUID, PARENT, "defined.json");
            },
        );
        test_modify_helper(
            "addattr-noindex",
            Expect::Pass,
            UUID,
            Some(PARENT.to_string()),
            None,
            Some("added-attr".to_string()),
            false,
            None,
            Some("added-attr-value".to_string()),
            false,
            false,
            |test| {
                test.populate_defined_device(UUID, PARENT, "defined.json");
            },
        );
        test_modify_helper(
            "mdev_type",
            Expect::Pass,
            UUID,
            Some(PARENT.to_string()),
            Some("changed-mdev-type".to_string()),
            None,
            false,
            None,
            None,
            false,
            false,
            |test| {
                test.populate_defined_device(UUID, PARENT, "defined.json");
            },
        );
        test_modify_helper(
            "multiple-noparent",
            Expect::Fail,
            UUID,
            None,
            None,
            None,
            false,
            None,
            None,
            true,
            false,
            |test| {
                test.populate_defined_device(UUID, PARENT, "defined.json");
                test.populate_defined_device(UUID, "0000:00:02.0", "defined.json");
            },
        );
        test_modify_helper(
            "multiple-parent",
            Expect::Pass,
            UUID,
            Some(PARENT.to_string()),
            None,
            None,
            false,
            None,
            None,
            true,
            false,
            |test| {
                test.populate_defined_device(UUID, PARENT, "defined.json");
                test.populate_defined_device(UUID, "0000:00:02.0", "defined.json");
            },
        );
        test_modify_helper(
            "auto-manual",
            Expect::Fail,
            UUID,
            Some(PARENT.to_string()),
            None,
            None,
            false,
            None,
            None,
            true,
            true,
            |_| {},
        );
    }

    fn test_undefine_helper<F>(
        testname: &str,
        expect: Expect,
        uuid: &str,
        parent: Option<String>,
        setupfn: F,
    ) where
        F: Fn(&TestEnvironment),
    {
        let test = TestEnvironment::new("undefine", testname);
        setupfn(&test);
        let uuid = Uuid::parse_str(uuid).unwrap();

        let result = crate::undefine_command(&test.env, uuid, parent.clone());

        if expect == Expect::Fail {
            result.expect_err("undefine command should have failed");
            return;
        }

        result.expect("undefine command should have succeeded");

        let devs = crate::defined_devices(&test.env, &Some(uuid), &parent)
            .expect("failed to query defined devices");
        assert!(devs.is_empty());
    }

    #[test]
    fn test_undefine() {
        init();

        const UUID: &str = "976d8cc2-4bfc-43b9-b9f9-f4af2de91ab9";
        const PARENT: &str = "0000:00:03.0";
        const PARENT2: &str = "0000:00:02.0";

        test_undefine_helper(
            "single",
            Expect::Pass,
            UUID,
            Some(PARENT.to_string()),
            |test| {
                test.populate_defined_device(UUID, PARENT, "defined.json");
            },
        );
        // if multiple devices with the same uuid exists, the one with the matching parent should
        // be undefined
        test_undefine_helper(
            "multiple-parent",
            Expect::Pass,
            UUID,
            Some(PARENT.to_string()),
            |test| {
                test.populate_defined_device(UUID, PARENT, "defined.json");
                test.populate_defined_device(UUID, PARENT2, "defined.json");
            },
        );
        // if multiple devices with the same uuid exists and no parent is specified, they should
        // all be undefined
        test_undefine_helper("multiple-noparent", Expect::Pass, UUID, None, |test| {
            test.populate_defined_device(UUID, PARENT, "defined.json");
            test.populate_defined_device(UUID, PARENT2, "defined.json");
        });
        test_undefine_helper(
            "nonexistent",
            Expect::Fail,
            UUID,
            Some(PARENT.to_string()),
            |_| {},
        );
    }
}
