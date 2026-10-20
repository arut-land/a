import os, pathlib, subprocess
root=pathlib.Path.cwd()
bench=str(root/'target/debug/build/a/66fa10731bfd94a9/out/lua_probe-66fa10731bfd94a9')
with open('target/lua-probe-results.txt','w') as out:
    subprocess.run(['uptime'],stdout=out)
    for candidate in ['baseline','lua54','luajit','luau','piccolo']:
        binary=root/'target/lua-probe-bins'/candidate
        out.write(f'{candidate} bytes={binary.stat().st_size}\n'); out.flush()
        for loaded in ([False] if candidate=='baseline' else [False, True]):
            for n in range(3):
                env=dict(os.environ,A_BIN=str(binary))
                if loaded: env['LUA_PROBE_LOADED']='1'
                out.write(f'{candidate} loaded={loaded} run={n}\n'); out.flush()
                subprocess.run([bench,'--ignored','--nocapture'],env=env,stdout=out,stderr=out,check=True)
