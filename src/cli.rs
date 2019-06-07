use crate::commands::classified::SinkCommand;
use crate::commands::command::sink;
use crate::commands::{autoview, tree};

use crate::prelude::*;

use crate::commands::classified::{
    ClassifiedCommand, ClassifiedInputStream, ClassifiedPipeline, ExternalCommand, InternalCommand,
    StreamNext,
};
use crate::context::Context;
crate use crate::errors::ShellError;
use crate::evaluate::Scope;

use crate::git::current_branch;
use crate::object::Value;
use crate::parser::ast::{Expression, Leaf, RawExpression};
use crate::parser::{Args, Pipeline};

use log::debug;
use rustyline::error::ReadlineError;
use rustyline::{self, ColorMode, Config, Editor};

use std::error::Error;
use std::iter::Iterator;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
pub enum MaybeOwned<'a, T> {
    Owned(T),
    Borrowed(&'a T),
}

impl<T> MaybeOwned<'a, T> {
    crate fn borrow(&self) -> &T {
        match self {
            MaybeOwned::Owned(v) => v,
            MaybeOwned::Borrowed(v) => v,
        }
    }
}

pub async fn cli() -> Result<(), Box<dyn Error>> {
    let mut context = Context::basic()?;

    {
        use crate::commands::*;

        context.add_commands(vec![
            command("ps", ps::ps),
            command("ls", ls::ls),
            command("cd", cd::cd),
            command("view", view::view),
            command("skip", skip::skip),
            command("first", first::first),
            command("size", size::size),
            command("from-json", from_json::from_json),
            command("from-toml", from_toml::from_toml),
            command("from-yaml", from_yaml::from_yaml),
            command("get", get::get),
            command("open", open::open),
            command("pick", pick::pick),
            command("split-column", split_column::split_column),
            command("split-row", split_row::split_row),
            command("reject", reject::reject),
            command("trim", trim::trim),
            command("to-array", to_array::to_array),
            command("to-json", to_json::to_json),
            command("to-toml", to_toml::to_toml),
            Arc::new(Where),
            Arc::new(Config),
            command("sort-by", sort_by::sort_by),
        ]);

        context.add_sinks(vec![
            sink("autoview", autoview::autoview),
            sink("tree", tree::tree),
        ]);
    }

    let config = Config::builder().color_mode(ColorMode::Forced).build();
    let h = crate::shell::Helper::new(context.clone_commands());
    let mut rl: Editor<crate::shell::Helper> = Editor::with_config(config);

    #[cfg(windows)]
    {
        let _ = ansi_term::enable_ansi_support();
    }

    rl.set_helper(Some(h));
    let _ = rl.load_history("history.txt");

    let ctrl_c = Arc::new(AtomicBool::new(false));
    let cc = ctrl_c.clone();
    ctrlc::set_handler(move || {
        cc.store(true, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    loop {
        if ctrl_c.load(Ordering::SeqCst) {
            ctrl_c.store(false, Ordering::SeqCst);
            if let ShellError::String(s) = ShellError::string("CTRL-C") {
                context.host.lock().unwrap().stdout(&format!("{:?}", s));
            }
            continue;
        }

        let readline = rl.readline(&format!(
            "{}{}> ",
            context.env.lock().unwrap().cwd().display().to_string(),
            match current_branch() {
                Some(s) => format!("({})", s),
                None => "".to_string(),
            }
        ));

        match process_line(readline, &mut context).await {
            LineResult::Success(line) => {
                rl.add_history_entry(line.clone());
            }

            LineResult::Error(err) => match err {
                ShellError::Diagnostic(diag, source) => {
                    let host = context.host.lock().unwrap();
                    let writer = host.err_termcolor();
                    let files = crate::parser::span::Files::new(source);

                    language_reporting::emit(
                        &mut writer.lock(),
                        &files,
                        &diag.diagnostic,
                        &language_reporting::DefaultConfig,
                    )
                    .unwrap();
                }

                ShellError::TypeError(desc) => context
                    .host
                    .lock()
                    .unwrap()
                    .stdout(&format!("TypeError: {}", desc)),

                ShellError::MissingProperty { subpath, .. } => context
                    .host
                    .lock()
                    .unwrap()
                    .stdout(&format!("Missing property {}", subpath)),

                ShellError::String(s) => context.host.lock().unwrap().stdout(&format!("{:?}", s)),
            },

            LineResult::Break => {
                break;
            }

            LineResult::FatalError(err) => {
                context
                    .host
                    .lock()
                    .unwrap()
                    .stdout(&format!("A surprising fatal error occurred.\n{:?}", err));
            }
        }
    }
    rl.save_history("history.txt").unwrap();

    Ok(())
}

enum LineResult {
    Success(String),
    Error(ShellError),
    Break,

    #[allow(unused)]
    FatalError(ShellError),
}

impl std::ops::Try for LineResult {
    type Ok = Option<String>;
    type Error = ShellError;

    fn into_result(self) -> Result<Option<String>, ShellError> {
        match self {
            LineResult::Success(s) => Ok(Some(s)),
            LineResult::Error(s) => Err(s),
            LineResult::Break => Ok(None),
            LineResult::FatalError(err) => Err(err),
        }
    }
    fn from_error(v: ShellError) -> Self {
        LineResult::Error(v)
    }

    fn from_ok(v: Option<String>) -> Self {
        match v {
            None => LineResult::Break,
            Some(v) => LineResult::Success(v),
        }
    }
}

async fn process_line(readline: Result<String, ReadlineError>, ctx: &mut Context) -> LineResult {
    match &readline {
        Ok(line) if line.trim() == "exit" => LineResult::Break,

        Ok(line) if line.trim() == "" => LineResult::Success(line.clone()),

        Ok(line) => {
            let result = match crate::parser::parse(&line) {
                Err(err) => {
                    return LineResult::Error(err);
                }

                Ok(val) => val,
            };

            debug!("=== Parsed ===");
            debug!("{:#?}", result);

            let mut pipeline = classify_pipeline(&result, ctx)?;

            match pipeline.commands.last() {
                Some(ClassifiedCommand::Sink(_)) => {}
                Some(ClassifiedCommand::External(_)) => {}
                _ => pipeline.commands.push(ClassifiedCommand::Sink(SinkCommand {
                    command: sink("autoview", autoview::autoview),
                    args: Args {
                        positional: vec![],
                        named: indexmap::IndexMap::new(),
                    },
                })),
            }

            let mut input = ClassifiedInputStream::new();

            let mut iter = pipeline.commands.into_iter().peekable();

            loop {
                let item: Option<ClassifiedCommand> = iter.next();
                let next: Option<&ClassifiedCommand> = iter.peek();

                input = match (item, next) {
                    (None, _) => break,

                    (Some(ClassifiedCommand::Expr(_)), _) => {
                        return LineResult::Error(ShellError::unimplemented(
                            "Expression-only commands",
                        ))
                    }

                    (_, Some(ClassifiedCommand::Expr(_))) => {
                        return LineResult::Error(ShellError::unimplemented(
                            "Expression-only commands",
                        ))
                    }

                    (Some(ClassifiedCommand::Sink(_)), Some(_)) => {
                        return LineResult::Error(ShellError::string("Commands like table, save, and autoview must come last in the pipeline"))
                    }

                    (Some(ClassifiedCommand::Sink(left)), None) => {
                        let input_vec: Vec<Value> = input.objects.collect().await;
                        left.run(
                            ctx,
                            input_vec,
                        )?;
                        break;
                    }

                    (
                        Some(ClassifiedCommand::Internal(left)),
                        Some(ClassifiedCommand::External(_)),
                    ) => match left.run(ctx, input).await {
                        Ok(val) => ClassifiedInputStream::from_input_stream(val),
                        Err(err) => return LineResult::Error(err),
                    },

                    (
                        Some(ClassifiedCommand::Internal(left)),
                        Some(_),
                    ) => match left.run(ctx, input).await {
                        Ok(val) => ClassifiedInputStream::from_input_stream(val),
                        Err(err) => return LineResult::Error(err),
                    },

                    (Some(ClassifiedCommand::Internal(left)), None) => {
                        match left.run(ctx, input).await {
                            Ok(val) => ClassifiedInputStream::from_input_stream(val),
                            Err(err) => return LineResult::Error(err),
                        }
                    }

                    (
                        Some(ClassifiedCommand::External(left)),
                        Some(ClassifiedCommand::External(_)),
                    ) => match left.run(ctx, input, StreamNext::External).await {
                        Ok(val) => val,
                        Err(err) => return LineResult::Error(err),
                    },

                    (
                        Some(ClassifiedCommand::External(left)),
                        Some(_),
                    ) => match left.run(ctx, input, StreamNext::Internal).await {
                        Ok(val) => val,
                        Err(err) => return LineResult::Error(err),
                    },

                    (Some(ClassifiedCommand::External(left)), None) => {
                        match left.run(ctx, input, StreamNext::Last).await {
                            Ok(val) => val,
                            Err(err) => return LineResult::Error(err),
                        }
                    }
                }
            }

            LineResult::Success(line.to_string())
        }
        Err(ReadlineError::Interrupted) => LineResult::Error(ShellError::string("CTRL-C")),
        Err(ReadlineError::Eof) => {
            println!("CTRL-D");
            LineResult::Break
        }
        Err(err) => {
            println!("Error: {:?}", err);
            LineResult::Break
        }
    }
}

fn classify_pipeline(
    pipeline: &Pipeline,
    context: &Context,
) -> Result<ClassifiedPipeline, ShellError> {
    let commands: Result<Vec<_>, _> = pipeline
        .commands
        .iter()
        .cloned()
        .map(|item| classify_command(&item, context))
        .collect();

    Ok(ClassifiedPipeline {
        commands: commands?,
    })
}

fn classify_command(
    command: &Expression,
    context: &Context,
) -> Result<ClassifiedCommand, ShellError> {
    // let command_name = &command.name[..];
    // let args = &command.args;

    if let Expression {
        expr: RawExpression::Call(call),
        ..
    } = command
    {
        match (&call.name, &call.args) {
            (
                Expression {
                    expr: RawExpression::Leaf(Leaf::Bare(name)),
                    ..
                },
                args,
            ) => match context.has_command(&name.to_string()) {
                true => {
                    let command = context.get_command(&name.to_string());
                    let config = command.config();
                    let scope = Scope::empty();

                    let args = match args {
                        Some(args) => config.evaluate_args(args.iter(), &scope)?,
                        None => Args::default(),
                    };

                    Ok(ClassifiedCommand::Internal(InternalCommand {
                        command,
                        args,
                    }))
                }
                false => match context.has_sink(&name.to_string()) {
                    true => {
                        let command = context.get_sink(&name.to_string());
                        let config = command.config();
                        let scope = Scope::empty();

                        let args = match args {
                            Some(args) => config.evaluate_args(args.iter(), &scope)?,
                            None => Args::default(),
                        };

                        Ok(ClassifiedCommand::Sink(SinkCommand { command, args }))
                    }
                    false => {
                        let arg_list_strings: Vec<String> = match args {
                            Some(args) => args.iter().map(|i| i.as_external_arg()).collect(),
                            None => vec![],
                        };

                        Ok(ClassifiedCommand::External(ExternalCommand {
                            name: name.to_string(),
                            args: arg_list_strings,
                        }))
                    }
                },
            },

            (_, None) => Err(ShellError::string(
                "Unimplemented command that is just an expression (1)",
            )),
            (_, Some(_)) => Err(ShellError::string("Unimplemented dynamic command")),
        }
    } else {
        Err(ShellError::string(&format!(
            "Unimplemented command that is just an expression (2) -- {:?}",
            command
        )))
    }
}
