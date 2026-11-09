// Copyright 2022 The AccessKit Authors. All rights reserved.
// Licensed under the Apache License, Version 2.0 or the MIT license.
// Morpheus: expose the existing string SetValue action through standard AT-SPI.

use accesskit_atspi_common::PlatformNode;
use zbus::{fdo, interface};

pub(crate) struct EditableTextInterface {
    node: PlatformNode,
}

impl EditableTextInterface {
    pub fn new(node: PlatformNode) -> Self {
        Self { node }
    }

    fn unsupported() -> fdo::Error {
        fdo::Error::NotSupported("This text editing operation is not supported.".into())
    }
}

#[interface(name = "org.a11y.atspi.EditableText")]
impl EditableTextInterface {
    fn set_text_contents(&self, new_contents: &str) -> fdo::Result<bool> {
        self.node
            .set_text_contents(new_contents)
            .map_err(|error| crate::util::map_error_from_node(&self.node, error))?;
        Ok(true)
    }

    fn copy_text(&self, _start_pos: i32, _end_pos: i32) -> fdo::Result<()> {
        Err(Self::unsupported())
    }

    fn cut_text(&self, _start_pos: i32, _end_pos: i32) -> fdo::Result<bool> {
        Err(Self::unsupported())
    }

    fn delete_text(&self, _start_pos: i32, _end_pos: i32) -> fdo::Result<bool> {
        Err(Self::unsupported())
    }

    fn insert_text(&self, _position: i32, _text: &str, _length: i32) -> fdo::Result<bool> {
        Err(Self::unsupported())
    }

    fn paste_text(&self, _position: i32) -> fdo::Result<bool> {
        Err(Self::unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::EditableTextInterface;
    use accesskit::{ActionHandler, ActionRequest, Node, NodeId, Role, Tree, TreeId, TreeUpdate};
    use accesskit_atspi_common::{
        Adapter, AdapterCallback, AppContext, Event, InterfaceSet, WindowBounds,
    };
    use std::sync::{Arc, Mutex};
    use zbus::fdo;

    struct Callback;

    impl AdapterCallback for Callback {
        fn register_interfaces(
            &self,
            _: &Adapter,
            _: accesskit_atspi_common::NodeId,
            _: InterfaceSet,
        ) {
        }
        fn unregister_interfaces(
            &self,
            _: &Adapter,
            _: accesskit_atspi_common::NodeId,
            _: InterfaceSet,
        ) {
        }
        fn emit_event(&self, _: &Adapter, _: Event) {}
    }

    struct Actions(Arc<Mutex<Vec<ActionRequest>>>);

    impl ActionHandler for Actions {
        fn do_action(&mut self, request: ActionRequest) {
            self.0.lock().unwrap().push(request);
        }
    }

    #[test]
    fn editable_text_protocol_accepts_only_supported_whole_replacement() {
        let mut root = Node::new(Role::Window);
        root.set_children(vec![NodeId(1)]);
        let mut input = Node::new(Role::TextInput);
        input.add_action(accesskit::Action::SetValue);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let adapter = Adapter::new(
            &AppContext::new(None),
            Callback,
            TreeUpdate {
                nodes: vec![(NodeId(0), root), (NodeId(1), input)],
                tree: Some(Tree::new(NodeId(0))),
                tree_id: TreeId::ROOT,
                focus: NodeId(1),
            },
            false,
            WindowBounds::default(),
            Actions(requests.clone()),
        );
        let interface = EditableTextInterface::new(
            adapter.platform_node(adapter.root_id().with_same_tree(NodeId(1))),
        );
        assert!(interface.set_text_contents("Synthetic replacement").unwrap());
        assert_eq!(requests.lock().unwrap().len(), 1);
        let copy: fdo::Result<()> = interface.copy_text(0, 1);
        assert!(matches!(copy, Err(fdo::Error::NotSupported(_))));
        for result in [
            interface.cut_text(0, 1),
            interface.delete_text(0, 1),
            interface.insert_text(0, "Synthetic", 9),
            interface.paste_text(0),
        ] {
            assert!(matches!(result, Err(fdo::Error::NotSupported(_))));
        }
        assert_eq!(requests.lock().unwrap().len(), 1);
        drop(adapter);
        assert!(interface.set_text_contents("Synthetic replacement").is_err());
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
}
