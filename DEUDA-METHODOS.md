# Deuda frente a methodOS — pendiente, no en curso

> **Este documento no pide acción inmediata.** Es el resultado de auditar este repo
> contra el estándar del namespace ([methodOS](https://github.com/enerBydev/methodos))
> el **2026-08-19**, anotado aquí para atacarlo cuando toque y no volver a
> descubrirlo desde cero. Nada de lo de abajo está en marcha.
>
> Contrato de referencia: `docs/kernel.md` (40 capacidades) y `docs/contrato-del-loop.md`
> (LOOP-1…5) del repo methodOS.

## Causa raíz: no hay `flake.nix`

Es el hallazgo que explica a casi todos los demás, así que va primero.

Este repo **no tiene devshell**. En consecuencia, en esta máquina no están instalados
`task`, `cargo-nextest`, `cargo-deny` ni `cargo-audit` — pese a que el repo versiona
`Taskfile.yaml`, `.nextest.toml` y `deny.toml`, y a que `CLAUDE.md` prescribe usarlos.

**El drift no fue descuido: es la consecuencia estructural de saltarse BUILD-1**
(construcción hermética, toda dependencia fijada). Sin entorno declarado, la
documentación describe una máquina que no existe, y no hay forma de que nadie se entere
hasta que falla.

Arreglo: `flake.nix` con rustc + las cuatro herramientas, `.envrc` con `use flake`.

## Drift medido en `CLAUDE.md` (245 líneas)

| Hallazgo | Medida exacta |
|---|---|
| Módulos `.rs` citados que no existen en `src/` | **1 de 25** — `prompt_cache.rs` |
| Comandos `task …` prescritos | **13**, todos presentes en `Taskfile.yaml`… |
| …pero el binario `task` | **no está instalado** → el flujo documentado es inejecutable tal cual |

La conclusión no es «hay que actualizarlo», es más incómoda: **un CLAUDE.md
enciclopédico no envejece neutro, empieza a mentirle al agente.** 245 líneas describiendo
comandos y módulos son 245 líneas de superficie que se desincroniza sola. La dirección de
arreglo es podar hacia lo que no puede mentir (qué es el proyecto, qué invariantes tiene)
y dejar que el flujo lo declare el archivo ejecutable.

Ver regla 7 de `docs/reglas-adopcion.md` en methodOS: *nada entra a `.claude/` hasta que
un error real lo justifique.*

## Decisión pendiente: `task` vs `just`

El namespace estandariza en `just` (capa de interfaz de methodOS); este repo usa
`Taskfile.yaml` con 13 targets **que funcionan y son más ricos que los de los `calc-*`**
(auto-version, service-logs, full-release, sync-binary).

No está decidido, y conviene no decidirlo por inercia. Las opciones, con su coste:

1. **Migrar a `just`** — uniforme con el namespace; hay que reescribir 13 targets que hoy
   funcionan y no han fallado.
2. **Mantener `task` como adaptador** — el kernel es agnóstico a la herramienta a
   propósito: exige el *verbo* (`ci`, `test`, `check`), no quién lo implementa. Un
   `justfile` de una línea por verbo delegando en `task` da uniformidad sin reescribir.

La opción 2 es más coherente con la doctrina («cambia el adaptador, no el contrato») y
más barata. **Aun así hay que verificar antes de decidir** que los verbos del kernel
tienen equivalente exacto en el Taskfile.

## Otras capacidades a revisar cuando se ataque

- **REL-5 / DEP-2** — comprobar si los releases pasan por un gate deliberado o se
  publican desde la estación de trabajo (fue el defecto que la autopsia de methodOS v1
  señaló, y este repo tiene `full-release` y `auto-version.yml`: hay que mirarlo, no
  suponerlo).
- **BUILD-4** — `deny.toml` está versionado; falta confirmar si `cargo-deny` corre en CI
  en cada build o solo existe el archivo.
- **OPS-3** — el propio kernel cita «los misterios de Nexus nunca investigados» como el
  fallo que justifica registrar causa raíz. Es el repo que originó la regla.
- **LOOP-5** — este archivo caduca: si sigue aquí sin cambios dentro de seis meses, o se
  ataca o se borra.

## Qué NO se tocó

Nada. Este documento es la única modificación de la auditoría. El código, el
`Taskfile.yaml`, los workflows y el `CLAUDE.md` quedan exactamente como estaban.
