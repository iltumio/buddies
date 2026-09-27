# Analisi della repository — 27 settembre 2026

## Ambito e metodo

Analisi mirata dell'architettura, delle dipendenze e dei percorsi di ingresso P2P,
ricerca, delega e persistenza. Non è un audit esaustivo né un penetration test.
Il grafo codebase-memory è stato usato per individuare simboli e relazioni;
la copertura inizialmente non disponibile e alcune relazioni imprecise sono
state compensate leggendo direttamente i percorsi rilevanti. Le osservazioni
sotto sono fondate sul codice, senza prove di sfruttamento su una rete reale.

## Architettura

Un singolo eseguibile Rust espone gli strumenti MCP via stdio o HTTP.
`BuddiesServer` traduce le chiamate MCP; `BuddiesNode` costruisce rete, storage
e watcher; `RoomManager` coordina gossip, identità, ricerche e task.
`Storage` usa redb e postcard. Le firme GPG/SSH passano da processi esterni.
Il watcher riconcilia il filesystem con Git e pubblica le differenze.

I moduli di dominio sono già separati dal trasporto. I test esistenti coprono,
tra l'altro, replay, falsificazione dell'attività dei peer, ordinamento delle
ricerche e riconciliazione del watcher. Il principale accentramento di
responsabilità è in `RoomManager`.

## Aggiornamenti applicati

| Dipendenza diretta | Prima | Dopo |
| --- | --- | --- |
| rmcp | 3.1.4 | 3.4.1 |
| iroh | 1.1.0 | 1.2.0 |
| redb | 4.2.0 | 4.3.0 |
| dirs | 6.0.0 | 7.0.0 |
| rand | 0.10.2 | 0.10.3 |
| uuid | 1.26.0 | 1.26.1 |

Le altre dipendenze dirette corrispondevano già alle versioni stabili più
recenti restituite da crates.io durante la verifica. Aggiornate anche le
transitive compatibili: il lockfile passa da 442 a 434 pacchetti.

- `rustls` passa da 0.23.43 a 0.23.45 e risolve
  [RUSTSEC-2026-0285 / GHSA-2mjx-qc3c-rqvc](https://github.com/rustls/rustls/security/advisories/GHSA-2mjx-qc3c-rqvc).
- `ServerInfo` è sostituito da `ServerConfig`, come richiesto dalla deprecazione
  di rmcp; il requisito minimo di rmcp è aggiornato a 3.4.1.
- Per `dirs 7` è stato confrontato il sorgente pubblicato con la versione 6:
  `data_local_dir`, unica API usata dal progetto, conserva il comportamento.
- Dichiarato `rust-version = "1.91"`, coerente con il README.
- Aggiornate le Actions: [checkout 7.0.1](https://github.com/actions/checkout/releases/tag/v7.0.1),
  [upload-artifact 7.0.1](https://github.com/actions/upload-artifact/releases/tag/v7.0.1),
  [download-artifact 8.0.1](https://github.com/actions/download-artifact/releases/tag/v8.0.1).
  I workflow usano runner GitHub `ubuntu-latest`; il layout degli artifact
  rimane compatibile con il comando di release esistente.
- Build, test e Clippy in CI usano `--locked`; i test usano `--all-targets`.

`cargo audit` dopo l'aggiornamento riporta zero vulnerabilità e due avvisi di
mancata manutenzione: `atomic-polyfill` tramite `postcard -> heapless`, e
`paste` tramite lo stack `iroh -> netwatch -> netlink-packet-core`.
Le catene sono state controllate con `cargo tree --target all`; lo stato finale
dopo le correzioni è descritto nella sezione sulla manutenzione.

## Correzioni implementate

Tutti e sei gli interventi funzionali del rapporto iniziale sono stati applicati.
Il formato postcard dei messaggi non è stato modificato; le verifiche all'ingresso
sono più restrittive.

| Area | Correzione | Evidenza |
| --- | --- | --- |
| Isolamento | Le ricerche ricevute impongono la stanza del trasporto; memorie, skill e task con stanze incongruenti vengono scartati; risultati e risposte ai task sono correlati anche alla stanza. Lo storage impedisce di spostare record esistenti tra stanze. | Test con due stanze, filtri omessi o falsificati, risposte nella stanza sbagliata. |
| Skill e voti | Validazione comune di hash, firma e policy per pubblicazioni e risposte; applicazione dei filtri originali; voto associato alla chiave firmataria normalizzata. Ranking ricalcolato dai voti locali, con `rank_source` esplicito. | Firme SSH reali, contenuto e firme alterati, ranking `i64::MAX` falsificato, voto ripetuto e falsificazione del votante. |
| Richieste pendenti | Registro dedicato con guardia RAII, chiave stanza/UUID, 128 richieste massime e timeout limitati. Nessun invio asincrono sotto mutex; i canali pieni scartano le risposte in eccesso. | Broadcast fallito, cancellazione di ricerca memorie/skill/task, saturazione e limite di capacità. |
| Lavoro bloccante | Facciata async dello storage con quattro slot `spawn_blocking`; GPG/SSH asincroni con quattro slot, timeout di 10 secondi e `kill_on_drop`. File temporanei privati con cleanup automatico. | Runtime responsivo durante lavori bloccanti, permit mantenuti dopo cancellazione, processo terminato al timeout, firme reali. |
| Ranking | Un'unica aggregazione dei voti nello snapshot della ricerca; letture puntuali del ranking tramite range sul prefisso; errori di lettura propagati. | Dataset di 200 skill/4.000 voti e test su dati corrotti. |
| Shutdown | SIGINT/SIGTERM, cancellazione delle sessioni e notifiche, drenaggio HTTP limitato a cinque secondi, arresto e attesa di watcher e ricevitori, chiusura router. | Watcher attivo, stream SSE aperto e processo HTTP terminato con codice zero; EOF stdio. |

Il registro è in `src/pending.rs`, la facciata redb in `src/async_storage.rs`
e la validazione comune delle skill in `src/validation.rs`. Join e Leave usano
anch'essi messaggi firmati; un errore del signer configurato non degrada più
silenziosamente a un messaggio senza firma.

## Compatibilità dei dati e del comportamento

- I vecchi voti non dimostrano chi li abbia espressi. La tabella originale
  `skill_votes` viene conservata, ma il ranking usa `verified_skill_votes_v2`.
  Occorre esprimere nuovamente i voti. Un test riapre un database con la tabella
  precedente e verifica sia la conservazione sia l'esclusione dal ranking.
- I voti richiedono SSH oppure un fingerprint GPG completo. Commenti SSH e
  differenze di maiuscole nel fingerprint non creano identità votanti aggiuntive.
- I peer precedenti possono ancora scambiare messaggi, ma i loro voti basati su
  endpoint ID vengono rifiutati. Conviene aggiornare tutti i peer prima di votare.
- Le ricerche espongono solo il ranking verificato localmente: può variare tra
  peer con una diversa cronologia di voti ricevuti.
- Poiché la tabella skill è indicizzata dall'hash globale, pubblicare lo stesso
  contenuto in una stanza diversa ora fallisce invece di spostare il record.
- Ricerca distribuita: massimo 30 secondi; delega: 1–300 secondi; long polling:
  massimo 30 secondi. Le risposte aggregate restano limitate a 50 risultati.

## Manutenzione automatica

Aggiunti Dependabot per Cargo e Actions, audit periodico e sulle modifiche alle
dipendenze, job Rust 1.91 e smoke test MCP in CI. Le Actions sono fissate a SHA
verificati tramite GitHub; `contents: write` è limitato al job di release.

La feature predefinita `heapless-cas` di postcard era attivata solo da buddies,
che usa esclusivamente API con allocazione. Disabilitarla elimina `heapless`
e l'avviso `atomic-polyfill`, senza modificare il formato serializzato.
Con la nuova dipendenza diretta `tempfile`, il lockfile finale contiene 431 pacchetti.
Resta l'avviso upstream `paste` attraverso `iroh -> netwatch -> netlink-packet-core`:
non esiste un aggiornamento compatibile risolto da Cargo che lo elimini. Non è
stato nascosto né aggirato introducendo un fork; l'audit periodico lo mantiene
visibile. Non risultano vulnerabilità note nel lockfile verificato.

## Misure e verifiche

- Il test `ranking_dataset_regression` misura la sola ricerca su 200 skill e
  4.000 voti, con build debug e database in memoria: circa 621 ms prima e 7 ms
  dopo. È una misura locale sintetica, non una garanzia sulle prestazioni in rete.
- `cargo test --locked --all-targets`: 67 test passati, nessuno ignorato.
- `cargo clippy --locked --all-targets -- -D warnings`, `cargo fmt --check`,
  `git diff --check`, `actionlint`: superati.
- `cargo +1.91 check --locked --all-targets`: superato.
- `cargo build --locked` e `python3 scripts/smoke_mcp.py`: superati. Lo script
  verifica inizializzazione, 22 tool, watcher e SSE attivi, SIGTERM HTTP e EOF
  stdio. Nella prova locale lo shutdown HTTP è durato circa un secondo.
- Audit: zero vulnerabilità, un avviso di mancata manutenzione (`paste`).

Le verifiche sono locali su Linux x86_64. I workflow remoti e la build ARM64 non
sono stati eseguiti. I test usano dati temporanei: il database personale non è
stato aperto né modificato. L'analisi rimane mirata ai punti del rapporto, non
costituisce un audit esaustivo del protocollo.
