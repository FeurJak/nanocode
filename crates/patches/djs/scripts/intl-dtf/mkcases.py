import sys
import json
L = ["de", "fr", "es", "ja", "zh", "pt-BR", "en-US"]
D = ["2024-03-10T09:59:59Z", "2024-03-31T01:00:00Z", "2024-10-27T00:59:59Z", "2024-11-03T09:00:00Z", "2024-06-15T12:34:56.789Z", "2025-01-01T00:00:00Z", "1990-04-05T12:00:00Z"]
Z = ["UTC", "America/Los_Angeles", "Europe/Berlin", "Asia/Tokyo", "Asia/Kolkata"]
def case(name, body):
    return {"id": "intl-dtf-" + name.replace("/", "-"), "kind": "js", "src": "const D=" + json.dumps(D) + ".map(s=>new Date(s));const out=[];" + body + "text(out.join('\\n'))"}
L2 = ["ko", "it", "ru", "nl", "pl", "tr", "ar", "ar-EG", "hi", "id", "vi", "zh-TW", "zh-Hant", "en-GB", "en-IN", "es-MX", "fr-CA"]
FIVE = ("/styles", "/month-names", "/weekday-names", "/parts", "/mixed")
cases = []
for l in L + L2:
    q = json.dumps(l)
    cases.append(case(l + "/styles", "for(const z of " + json.dumps(Z) + ")for(const ds of [undefined,'full','long','medium','short'])for(const ts of [undefined,'full','long','medium','short']){if(!ds&&!ts)continue;const f=new Intl.DateTimeFormat(" + q + ",{dateStyle:ds,timeStyle:ts,timeZone:z});out.push(z+' '+ds+' '+ts+' '+D.map(d=>f.format(d)).join(' | '))}"))
    cases.append(case(l + "/month-names", "for(const w of ['long','short','narrow']){const a=[],b=[];for(let m=0;m<12;m++){const d=new Date(Date.UTC(2024,m,5));a.push(new Intl.DateTimeFormat(" + q + ",{month:w,timeZone:'UTC'}).format(d));b.push(new Intl.DateTimeFormat(" + q + ",{month:w,day:'numeric',timeZone:'UTC'}).format(d))}out.push(w+': '+a.join(',')+' / '+b.join(','))}"))
    cases.append(case(l + "/weekday-names", "for(const w of ['long','short','narrow']){const a=[],b=[];for(let i=0;i<7;i++){const d=new Date(Date.UTC(2024,2,3+i));a.push(new Intl.DateTimeFormat(" + q + ",{weekday:w,timeZone:'UTC'}).format(d));b.push(new Intl.DateTimeFormat(" + q + ",{weekday:w,month:'long',day:'numeric',timeZone:'UTC'}).format(d))}out.push(w+': '+a.join(',')+' / '+b.join(','))}"))
    cases.append(case(l + "/parts", "const os=[{weekday:'long',year:'numeric',month:'long',day:'numeric'},{era:'short',year:'numeric'},{era:'long',year:'numeric',month:'short'},{hour:'numeric',minute:'2-digit',hour12:true},{hour:'numeric',dayPeriod:'long'},{hour:'2-digit',minute:'2-digit',second:'2-digit',fractionalSecondDigits:2,hourCycle:'h23'},{month:'narrow',year:'2-digit'},{weekday:'short',hour:'numeric',minute:'numeric'}];for(const o of os){const f=new Intl.DateTimeFormat(" + q + ",{...o,timeZone:'Asia/Kolkata'});out.push(JSON.stringify(o)+' '+JSON.stringify(f.formatToParts(D[4]))+' '+JSON.stringify(f.resolvedOptions()))}const bc=new Date(Date.UTC(2000,0,1));bc.setUTCFullYear(-44);out.push(new Intl.DateTimeFormat(" + q + ",{era:'long',year:'numeric',timeZone:'UTC'}).format(bc))"))
    cases.append(case(l + "/zones", "for(const z of " + json.dumps(Z + ["Europe/London", "Australia/Sydney", "+05:30"]) + ")for(const k of ['short','long','shortOffset','longOffset','shortGeneric','longGeneric']){const f=new Intl.DateTimeFormat(" + q + ",{timeZone:z,timeZoneName:k,hour:'numeric',minute:'2-digit'});out.push(z+' '+k+' '+D.slice(0,4).map(d=>f.format(d)).join(' | '))}"))
    cases.append(case(l + "/toLocale", "for(const z of " + json.dumps(Z) + ")for(const d of D)out.push(z+' '+d.toLocaleString(" + q + ",{timeZone:z})+' | '+d.toLocaleDateString(" + q + ",{timeZone:z})+' | '+d.toLocaleTimeString(" + q + ",{timeZone:z})+' | '+d.toLocaleDateString(" + q + ",{timeZone:z,weekday:'long',month:'long',day:'numeric',year:'numeric'}))"))
    cases.append(case(l + "/mixed", "const os=[{month:'long',day:'numeric',hour:'numeric',minute:'2-digit'},{month:'short',day:'numeric',year:'numeric',hour:'numeric',minute:'2-digit',timeZoneName:'short'},{weekday:'long',hour:'numeric',minute:'2-digit'},{year:'numeric',month:'2-digit',day:'2-digit',hour:'2-digit',minute:'2-digit',second:'2-digit',hour12:false},{month:'long',year:'numeric'},{hour:'numeric',hourCycle:'h11'},{hour:'numeric',hourCycle:'h24',minute:'numeric'},{dayPeriod:'short',hour:'numeric',minute:'numeric'}];for(const o of os){const f=new Intl.DateTimeFormat(" + q + ",{...o,timeZone:'Europe/Berlin'});out.push(JSON.stringify(o)+' '+D.map(d=>f.format(d)).join(' | '))}"))
keep = {"intl-dtf-" + (l + n).replace("/", "-") for l in L for n in FIVE + ("/toLocale",)} | {"intl-dtf-" + (l + n).replace("/", "-") for l in L2 for n in FIVE}
cases = [c for c in cases if c["id"] in keep]
open(sys.argv[1], "w").write("".join(json.dumps(c, ensure_ascii=False) + "\n" for c in cases))
print(len(cases))
