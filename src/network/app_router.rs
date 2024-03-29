
use std::{str::FromStr, sync::Arc, time::UNIX_EPOCH};

use axum::{body::{self, Body, to_bytes}, extract::Request, http::request::Parts, response::IntoResponse, routing::{delete, get, patch, post, put}, RequestPartsExt, Router};
use bollard::{auth};
use hyper::{upgrade::Upgraded, HeaderMap, Method, Response, StatusCode, Uri, Version};
use mongodb::{bson::{bson, doc, oid::ObjectId, Bson}, Database};
use bytes::Bytes;
use http_body_util::{combinators::{BoxBody, UnsyncBoxBody}, BodyExt, Empty, Full};

use hyper::service::service_fn;
use hyper::{client::conn::http1};
use hyper_util::rt::tokio::TokioIo;


use tokio::{io::{self, AsyncReadExt, AsyncWriteExt as _}, net::{TcpListener, TcpStream}};

use crate::{models::docker_models::{Container, LoadBalancer}, utils::{docker_utils::{get_load_balancer_instances, route_container, route_load_balancer, try_start_container}, mongodb_utils::{DBCollection, DATABASE}}};
use crate::models::docker_models::{ContainerRoute};


pub async fn router()->axum::Router {
    
    let router = Router::new()
        .route("/*path",
        get(active_service_discovery)
        .patch(active_service_discovery)
        .post(active_service_discovery)
        .put(active_service_discovery)
        .delete(active_service_discovery)
        );
        
    return router;
}

pub async fn active_service_discovery(request: Request<Body>) 
-> impl IntoResponse
{
    println!("recieved request");
    println!("request:{:#?}",&request);
    
    let (parts,body) = request.into_parts();
    let response = match route_identifier(parts.uri.path_and_query().unwrap().to_string()).await {
        Some(docker_image) => {
            println!("{}", docker_image);
            //check instances of the load_balancer
            let load_balancer =get_load_balancer_instances(docker_image).await;

            let port_forward_result = port_forward_request(load_balancer, parts, body).await;
            port_forward_result.into_response()
        },
        None => {
            (StatusCode::NOT_FOUND).into_response()
        }
    };
    return response;
}




///returns the Router Docker Image 
pub async fn route_identifier(uri:String) -> Option<String>{
    
    let database: &Database = DATABASE.get().unwrap();
    let collection_name = DBCollection::ROUTES.to_string();
    let collection = database.collection::<ContainerRoute>(collection_name.as_str());
    let mut cursor: mongodb::Cursor<ContainerRoute> = collection.find( 
        doc! {
            "$expr": {
                "$eq": [
                    {
                        "$indexOfBytes": [
                            uri.clone(),
                            "$address"
                           
                        ]
                    },
                    0
                ]
            }
          }, None).await.unwrap();
    
    let mut container_route_matches: Vec<ContainerRoute> = Vec::new();
    while cursor.advance().await.unwrap() {
        let document_item: Result<ContainerRoute, mongodb::error::Error> = cursor.deserialize_current();
        match document_item {
            Ok(document) => {
                container_route_matches.push(document);
            }
            Err(_) =>{}
        }        
    }
    if container_route_matches.len() == 0 { //no matching routes
        return None
    }else if container_route_matches.len() == 1 {
        return Some(container_route_matches[0].image_name.clone());
    }
    else{
        return Some(route_resolver(container_route_matches, uri))
    }
        
}
///helper function to help resolve multiple route results
pub fn route_resolver(container_route_matches:Vec<ContainerRoute>, uri:String) -> String{

    let routes:Vec<Vec<String>> = container_route_matches.iter().map(|container_route| {
        let route:Vec<String> = container_route.address.split("/").filter(|s| s.to_owned()!="").map(String::from).collect();
        route
    }).collect();
    //need to optimize/gets running per split instead of generally at the end
    let uri_split:Vec<String> = uri.split("/").filter(|x| x.to_owned() != "").map(|x| {
        let split_strings = vec!["?", "#"];
        let mut clone_string = x.to_owned();
        for split_string in split_strings{
            clone_string = clone_string.split(split_string).into_iter().collect::<Vec<&str>>()[0].to_string();
        }
        let ret_string: String = clone_string.clone();
        return ret_string
    }).into_iter().collect();

    
    let mut matched_index:usize = 0;
    let mut max_matches:usize = 0;
    for (container_index, container_route) in routes.iter().enumerate() {
        //let mut current_matches:usize = 0;
        let minimun_matches:usize = container_route.len();
        if uri_split.starts_with(container_route) && minimun_matches > max_matches{
            matched_index = container_index;
            max_matches = minimun_matches
        }
    }
    return container_route_matches[matched_index].image_name.clone();
}

pub async fn port_forward_request(load_balancer:LoadBalancer, parts:Parts, body:Body) -> impl IntoResponse{
    let database = DATABASE.get().unwrap();
    let container_id = route_container(load_balancer).await;
    let object_id:ObjectId = ObjectId::from_str(container_id.as_str()).unwrap();
    println!("{:#?}",object_id);
    let container_result = database.collection::<Container>(DBCollection::CONTAINERS.to_string().as_str()).find_one(doc! {"_id": object_id}, None).await.unwrap().unwrap();
    //try to start the container if not starting
    let forward_request_result = match try_start_container(container_result.container_id).await {
        Ok(_)=>{
            println!("started container");
            //let _ = handshake_and_send(parts, body, container_result.public_port).await;
            let forward_result = forward_request(parts, body, container_result.public_port).await.into_response();
            forward_result
        },
        Err(err)=>{
            //cannot start container
            println!("CANNOT START CONTAINER");
            let res = (StatusCode::INTERNAL_SERVER_ERROR,err).into_response();
            res
        }
    };
    forward_request_result
}

pub async fn handshake_and_send(parts:Parts, body:Body, public_port:usize){
   
    //open a TCP connection to the remote host
    let url = parts.headers.get::<&str>("host").unwrap();
    let host = parts.headers.get("host").unwrap().to_str().unwrap().split(":").collect::<Vec<&str>>()[0];
    //let host = "192.168.254.106";
    //let address_str = format!("https://{}:{}{}", host, public_port, parts.uri );
    let address_str = format!("{}:{}", host, public_port);

    let remote_url = url.to_str().unwrap().parse::<hyper::Uri>().unwrap();
    
    
    match TcpStream::connect(address_str.clone()).await {
        Ok(stream) =>{
            let io = TokioIo::new(stream);
            let (mut sender, conn) =  hyper::client::conn::http1::handshake(io).await.unwrap();
            tokio::task::spawn(async move {
                if let Err(err) = conn.await {
                    println!("Connection failed: {:?}", err);
                }
            });
           
            match Uri::builder()
            .scheme("https")
            .authority(address_str.clone())
            .path_and_query(parts.uri.to_string())
            .build() {
                Ok(uri)=>{
                    let authority = uri.authority().unwrap().clone();
                    println!("uri: {:#?}",uri);
                    match Request::builder()
                    .uri(uri.path())
                    .method(Method::GET)
                    .header(hyper::header::HOST, authority.as_str())
                    .body(Empty::<Bytes>::new()) {
                        Ok(req)=>{
                            println!("req: {:#?}",req);
                            println!("uri: {:#?}", uri);                  
                            let mut res =  sender.send_request(req).await ; //sender and connection for the handshake result
                            println!("res: {:#?}", res); //incomplete message here
                            
                        }
                        Err(e)=>{
                            println!("{:#?}",e);
                        }
                    }
                    
                },
                Err(e)=>{println!("error: {}",e)}
            };
        
            
            
            // Stream the body, writing each frame to stdout as it arrives
            
        },
        Err(e)=>{
            //cannot create a tcp_stream connection
            println!("cannot create a tcp_stream_connection:{}",e);

        }
    }
    
    
}

pub async fn forward_request(parts:Parts, body:Body, public_port:usize)
-> impl IntoResponse
{
    
    let mut forward_attempt = 1;
    let time = std::time::SystemTime::now();
    let current_time = time.duration_since(UNIX_EPOCH).unwrap().as_secs();
    let mut attempt_time = time.duration_since(UNIX_EPOCH).unwrap().as_secs();
    let maximum_time_attempt_in_seconds:u64 = 3 * 1000;
    println!("forward request");
    let client_builder = reqwest::ClientBuilder::new();
    let client = client_builder.use_rustls_tls().danger_accept_invalid_certs(true).build().unwrap();
    let bytes = to_bytes(body, usize::MAX).await.unwrap();

    let uri = parts.uri;
    let url = format!("https://localhost:{}{}",public_port,uri);
    println!("headers:{:#?}",parts.headers);
    
    loop { //try to connect till it becomes OK
        attempt_time = time.duration_since(UNIX_EPOCH).unwrap().as_secs();
        if attempt_time - current_time < maximum_time_attempt_in_seconds {
            let method_result = match parts.method {
                Method::GET => {Ok(client.get(&url).send().await)},
                Method::DELETE => {Ok(client.delete(&url).send().await)},
                //Method::PATCH => {Ok(client.patch(url).body(b).send().await)},
                Method::POST => {Ok(client.post(&url).headers(parts.headers.clone()).body(bytes.clone()).send().await)},
                //Method::PUT => {Ok(client.post(url).body(body).send().await)}
                _ => {
                    //unhandled method. what to return?
                    Err((StatusCode::INTERNAL_SERVER_ERROR).into_response())
                    
                }
            };
            match method_result {
                Ok(mr_ok) => {
                    let mr_ok_res = match mr_ok {
                        Ok(result) => {
                            let status = &result.status();
                            let res = result.text_with_charset("utf-8").await;
                            return match res {
                                Ok(res_body) => (*status,res_body).into_response(),
                                Err(res_error) => (*status, res_error.to_string()).into_response()
                            };
                              
                        }
                        Err(error) => {
                            println!("error{:#?}",error);
                            
                            if error.status().is_some(){
                                (error.status().unwrap(), error.to_string()).into_response()
                            }else{
                                (StatusCode::INTERNAL_SERVER_ERROR).into_response()
                            }
                        }
                    };
                },
                Err(mr_err)=>{
                    println!("mr:err{:#?}",mr_err)
                }
            };
        }else{
            return (StatusCode::REQUEST_TIMEOUT).into_response()
        }
       
        // if handshake_success {
        //     return res;
        // }else{}
    }
    
    
        
}
