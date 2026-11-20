import configparser,json,os,subprocess,tempfile,unittest
from pathlib import Path

BASE=Path(__file__).resolve().parent
class InstalledHost(unittest.TestCase):
    def test_installed_launcher_preserves_legacy_work_and_forces_paired_tls(self):
        unit=configparser.ConfigParser(interpolation=None);unit.read(BASE/'enzimos-phone-link.service')
        self.assertNotIn('enzremote-host.service',unit.get('Unit','Conflicts',fallback='').split())
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);control=root/'systemctl';host=root/'enzremote-signaling'
            control.write_text('#!/usr/bin/bash\nexit "$QA_LEGACY_STATUS"\n');control.chmod(0o755)
            host.write_text('#!/usr/bin/python3\nimport json,os\nprint(json.dumps({key:os.environ.get(key) for key in os.environ if key.startswith("ENZREMOTE_")}))\n');host.chmod(0o755)
            base=['bwrap','--unshare-all','--die-with-parent','--new-session','--clearenv',
                '--dir','/usr','--ro-bind','/usr/bin','/usr/bin','--ro-bind','/usr/lib','/usr/lib',
                '--symlink','usr/bin','/bin','--symlink','usr/lib','/lib','--symlink','usr/lib','/lib64',
                '--dev','/dev','--proc','/proc','--dir','/usr/libexec/enzimos-phone-link',
                '--ro-bind',str(control),'/usr/bin/systemctl','--ro-bind',str(host),'/usr/libexec/enzimos-phone-link/enzremote-signaling',
                '--ro-bind',str(BASE/'start-host'),'/start-host','--setenv','PATH','/usr/bin:/bin',
                '--setenv','ENZREMOTE_TLS','off','--setenv','ENZREMOTE_ALLOW_LEGACY_COMMANDS','1',
                '--setenv','ENZREMOTE_ALLOW_ANONYMOUS_LOOPBACK','1','--setenv','ENZREMOTE_ALLOW_LAN_PLAINTEXT','1',
                '--setenv','ENZREMOTE_REQUIRE_PAIRING','0','--setenv','ENZREMOTE_REQUIRE_ACTIVE_SESSION','0']
            for status in ('0','1','3'):
                result=subprocess.run([*base,'--setenv','QA_LEGACY_STATUS',status,'--','/usr/bin/bash','/start-host'],capture_output=True,text=True,timeout=10)
                if status in ('0','1'):
                    self.assertEqual(result.returncode,78,result.stderr);self.assertFalse(result.stdout)
                else:
                    self.assertEqual(result.returncode,0,result.stderr);values=json.loads(result.stdout)
                    for key,value in {'TLS':'on','ALLOW_LEGACY_COMMANDS':'0','ALLOW_ANONYMOUS_LOOPBACK':'0','ALLOW_LAN_PLAINTEXT':'0','REQUIRE_PAIRING':'1','REQUIRE_ACTIVE_SESSION':'1','CLIPBOARD_BACKEND':'shell'}.items():
                        self.assertEqual(values['ENZREMOTE_'+key],value,key)

if __name__=='__main__':unittest.main()
