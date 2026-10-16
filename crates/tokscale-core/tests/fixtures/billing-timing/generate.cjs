const fs=require('node:fs'),path=require('node:path'),{spawnSync}=require('node:child_process');
const out=__dirname;
const [kind,bin]=process.argv.slice(2);
if(!['timing','billing'].includes(kind)||!bin)throw Error('Usage: node generate.cjs <timing|billing> <binary>');
fs.mkdirSync(out,{recursive:true});
{
 const home='/private/tmp/tm-tokscale-cache-fixture-'+kind;
 if(fs.existsSync(home))throw Error('Fixture home already exists: '+home); fs.mkdirSync(home,{recursive:true});
 const dir=path.join(home,'.claude/projects/-tmp-fixture'); fs.mkdirSync(dir,{recursive:true});
 const rows=[{type:'assistant',sessionId:'retained-fixture',uuid:'assistant-1',timestamp:'2026-10-01T12:00:00Z',cwd:'/tmp/fixture',message:{id:'msg-fixture',role:'assistant',model:'claude-sonnet-4-5',usage:{input_tokens:100,output_tokens:20,cache_read_input_tokens:30,cache_creation_input_tokens:10}}}];
 fs.writeFileSync(path.join(dir,'retained-fixture.jsonl'),rows.map(JSON.stringify).join('\n')+'\n');
 const ampDir=path.join(home,'.local/share/amp/threads');fs.mkdirSync(ampDir,{recursive:true});
 fs.writeFileSync(path.join(ampDir,'T-fixture.json'),JSON.stringify({id:'amp-fixture',created:1790856000000,messages:[{role:'assistant',messageId:1,usage:{model:'claude-sonnet-4-0',inputTokens:100,outputTokens:20,credits:0.75}}]}));
 const mimoDir=path.join(home,'.local/share/mimocode');fs.mkdirSync(mimoDir,{recursive:true});
 const db=new (require('node:sqlite').DatabaseSync)(path.join(mimoDir,'mimocode.db'));
 db.exec('CREATE TABLE message (id TEXT PRIMARY KEY,session_id TEXT NOT NULL,data TEXT NOT NULL)');
 db.prepare('INSERT INTO message VALUES (?,?,?)').run('mimo-row','mimo-fixture',JSON.stringify({role:'assistant',modelID:'mimo-v2.5-pro',providerID:'mimo',cost:0.75,tokens:{input:100,output:20,reasoning:0,cache:{read:0,write:0}},time:{created:1790856000000}}));db.close();
 const config=path.join(home,'.config/tokscale');fs.mkdirSync(path.join(config,'cache'),{recursive:true});
 for(const source of ['litellm','openrouter','models-dev'])fs.writeFileSync(path.join(config,'cache/pricing-'+source+'.json'),JSON.stringify({timestamp:Math.floor(Date.now()/1000),data:{}}));
 const env={...process.env,HOME:home,USERPROFILE:home,TOKSCALE_CONFIG_DIR:config,TOKSCALE_PRICING_CACHE_ONLY:'1',XDG_CONFIG_HOME:path.join(home,'.config'),XDG_DATA_HOME:path.join(home,'.local/share'),XDG_CACHE_HOME:path.join(home,'.cache'),HTTP_PROXY:'http://127.0.0.1:9',HTTPS_PROXY:'http://127.0.0.1:9'};
 for(const key of ['TOKSCALE_EXTRA_DIRS','HERMES_HOME','DSH_HOME','TOKSCALE_HEADLESS_DIR'])delete env[key];
 const result=spawnSync(bin,['--json','--client','claude,amp,micode','--group-by','client,model','--home',home,'--no-spinner'],{env,encoding:'utf8',timeout:30000});
 console.log(kind,result.status,result.stderr.slice(0,500)); if(result.status!==0)throw Error(result.stdout);
 fs.writeFileSync(path.join(out,kind+'-report.json'),result.stdout);
 function walk(p){for(const e of fs.readdirSync(p,{withFileTypes:true})){const f=path.join(p,e.name);if(e.isDirectory())walk(f);else if(e.name.endsWith('.bin')){const namespace=f.includes('/claude/')?'claude':f.includes('/hermes/')?'hermes':f.includes('/amp/')?'amp':f.includes('/micode/')?'micode':null;if(namespace){fs.copyFileSync(f,path.join(out,kind+'-'+namespace+'.bin'));console.log(namespace,f);}}}}
 walk(config);
}
