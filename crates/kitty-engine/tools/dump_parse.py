"""Reference dump of the m0on KittyPatcher 0.7.6 v2 parse step (`load_mods`) for a mods folder.

Imports the Python patcher as a module and calls ONLY load_mods (no HTML is read or patched). The mods are
copied into a temporary work directory first and the process chdirs there, so nothing in the real stage
folders is touched.

usage: python dump_parse.py <mods_dir> <out.json> [patcher_dir]
patcher_dir defaults to "M0on-Kittypatcher v0.7.6 v2" next to the fork (kittypatcher dev folder).
"""
import glob
import importlib.util
import json
import os
import shutil
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
src_mods, out_json = sys.argv[1], os.path.abspath(sys.argv[2])
PATCHER_DIR = (
    sys.argv[3]
    if len(sys.argv) > 3
    else os.path.normpath(os.path.join(HERE, "..", "..", "..", "..", "M0on-Kittypatcher v0.7.6 v2"))
)

work = tempfile.mkdtemp(prefix="kitty-ref-")
os.makedirs(os.path.join(work, "mods"))
for f in glob.glob(os.path.join(src_mods, "*")):
    if os.path.isfile(f) and f.lower().endswith((".mod", ".kdiff", ".patch", ".json")):
        shutil.copy(f, os.path.join(work, "mods"))
with open(os.path.join(work, "CourseOfTemptation.html"), "w") as fh:
    fh.write("<html></html>")
os.chdir(work)
sys.path.insert(0, PATCHER_DIR)
sys.argv = ["kp", os.path.join(work, "CourseOfTemptation.html")]

spec = importlib.util.spec_from_file_location("kp2", os.path.join(PATCHER_DIR, "M0on-KittyPatcherv0.7.6_v2.py"))
M = importlib.util.module_from_spec(spec)
spec.loader.exec_module(M)
M.handle_output = lambda *a, **k: None

(mod_dict, mod_list, mod_file_indexes, ok, mod_reg_list, mod_struct_list, mod_func_list, mod_hook_list,
 conflict_map, passage_registry, passage_names_seen, js_registry, css_registry, events_registry,
 key_to_mod) = M.load_mods(os.path.join(work, "mods"))


def conv(o):
    if isinstance(o, (set, frozenset)):
        return sorted(o)
    if isinstance(o, tuple):
        return [conv(x) for x in o]
    if isinstance(o, dict):
        return {str(k): conv(v) for k, v in o.items()}
    if isinstance(o, list):
        return [conv(x) for x in o]
    return o


out = {
    "loaded_mods": ok,
    "mod_dict": [[k, conv(v)] for k, v in mod_dict.items()],
    "mod_list": conv(mod_list),
    "mod_file_indexes": conv(mod_file_indexes),
    "mod_reg_list": conv(mod_reg_list),
    "mod_struct_list": conv(mod_struct_list),
    "mod_func_list": conv(mod_func_list),
    "mod_hook_list": conv(mod_hook_list),
    "conflict_map": conv(conflict_map),
    "passage_registry": conv(passage_registry),
    "passage_names_seen": conv(passage_names_seen),
    "js_registry": conv(js_registry),
    "css_registry": conv(css_registry),
    "events_registry": conv(events_registry),
    "key_to_mod": conv(key_to_mod),
}
with open(out_json, "w", encoding="utf-8") as fh:
    json.dump(out, fh, ensure_ascii=False, indent=1)
shutil.rmtree(work, ignore_errors=True)
print("mods", len(ok), "| mod_dict", len(mod_dict), "| mod_list", len(mod_list), "| reg", len(mod_reg_list),
      "| func", len(mod_func_list), "| passages", len(passage_registry), "| js", len(js_registry),
      "| css", len(css_registry), "| events", len(events_registry))
